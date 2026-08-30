//! Integration tests for `Mds::export_set`.
//!
//! Phase 5.7 ships the library impl alongside the `cosmix-mds export`
//! CLI. CLI shape is covered in `tests/cli.rs`. These tests pin the
//! library contract: archive layout, manifest fields, distinct-by-hash
//! blob selection, and refusal to produce a partial archive when a
//! referenced CAS file is missing.

use cosmix_mds::{ContainerAttrs, Flags, Mds, Membership, SetId, SqliteCasMds};
use std::collections::BTreeSet;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use tempfile::TempDir;

fn open() -> (TempDir, SqliteCasMds) {
    let d = TempDir::new().unwrap();
    let mds = SqliteCasMds::open(d.path()).unwrap();
    (d, mds)
}

fn fresh_set() -> SetId {
    SetId(uuid::Uuid::now_v7())
}

fn attrs() -> ContainerAttrs {
    ContainerAttrs {
        special_use: None,
        subscribed: true,
        extra: serde_json::json!({}),
    }
}

fn read_tar_entries(path: &Path) -> Vec<(String, Vec<u8>)> {
    let f = File::open(path).unwrap();
    let mut ar = tar::Archive::new(f);
    let mut out = Vec::new();
    for e in ar.entries().unwrap() {
        let mut e = e.unwrap();
        let name = e.path().unwrap().to_string_lossy().into_owned();
        let mut bytes = Vec::new();
        e.read_to_end(&mut bytes).unwrap();
        out.push((name, bytes));
    }
    out
}

#[test]
fn export_writes_manifest_data_and_referenced_blobs() {
    let (d, mds) = open();
    let s = fresh_set();
    mds.create_set(&s).unwrap();
    let inbox = mds.create_container(&s, None, "INBOX", attrs()).unwrap();

    let b1 = mds.put_blob(b"alpha").unwrap();
    let b2 = mds.put_blob(b"bravo").unwrap();
    for h in [&b1, &b2] {
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

    let dest = d.path().join("export.tar");
    let report = mds.export_set(&s, &dest).unwrap();

    assert_eq!(report.set_id, s);
    assert_eq!(report.item_count, 2);
    assert_eq!(report.blob_count, 2);
    assert!(report.bytes_written > 0);
    assert!(dest.exists());

    let entries = read_tar_entries(&dest);
    let names: BTreeSet<&str> = entries.iter().map(|(n, _)| n.as_str()).collect();
    assert!(names.contains("manifest.json"), "names: {names:?}");
    assert!(names.contains("data.sqlite"), "names: {names:?}");

    // First entry is the manifest — pin the streaming-friendly order.
    assert_eq!(
        entries[0].0, "manifest.json",
        "first entry should be manifest"
    );

    // Manifest fields.
    let manifest_bytes = &entries[0].1;
    let v: serde_json::Value = serde_json::from_slice(manifest_bytes).unwrap();
    assert_eq!(v["format"], "cosmix-mds-set-export");
    assert_eq!(v["format_version"], 1);
    assert_eq!(v["set_id"], s.0.to_string());
    // Fresh DB exports at the latest schema (DATA_LATEST, now v9 after
    // the v1.8 item-keyed sidecar FK-cascade migration).
    assert_eq!(v["schema_version"], 9);
    assert_eq!(v["item_count"], 2);
    assert_eq!(v["blob_count"], 2);
    assert!(v["exported_at"].as_str().unwrap().contains("T"));

    // Both blobs present at sharded paths.
    let mut blob_count = 0;
    for (name, _) in &entries {
        if name.starts_with("blobs/") {
            blob_count += 1;
        }
    }
    assert_eq!(blob_count, 2);
}

#[test]
fn export_dedupes_blobs_by_hash() {
    // Two items referencing the same hash → blob_count=1 in the
    // manifest and exactly one blob entry in the tarball, but
    // item_count=2 because the dedup is at the *blob* layer, not
    // the item layer.
    let (d, mds) = open();
    let s = fresh_set();
    mds.create_set(&s).unwrap();
    let inbox = mds.create_container(&s, None, "INBOX", attrs()).unwrap();

    let h = mds.put_blob(b"shared").unwrap();
    for _ in 0..2 {
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
    }

    let dest = d.path().join("dedup.tar");
    let report = mds.export_set(&s, &dest).unwrap();
    assert_eq!(report.item_count, 2);
    assert_eq!(report.blob_count, 1);

    let entries = read_tar_entries(&dest);
    let blob_entries: Vec<&str> = entries
        .iter()
        .map(|(n, _)| n.as_str())
        .filter(|n| n.starts_with("blobs/"))
        .collect();
    assert_eq!(
        blob_entries.len(),
        1,
        "expected one blob, got {blob_entries:?}"
    );
}

#[test]
fn export_with_no_items_writes_only_manifest_and_data() {
    // A freshly-created set with no items still exports cleanly:
    // manifest + data.sqlite, no blob entries. blob_count = 0.
    let (d, mds) = open();
    let s = fresh_set();
    mds.create_set(&s).unwrap();

    let dest = d.path().join("empty.tar");
    let report = mds.export_set(&s, &dest).unwrap();
    assert_eq!(report.item_count, 0);
    assert_eq!(report.blob_count, 0);

    let entries = read_tar_entries(&dest);
    let names: Vec<&str> = entries.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, vec!["manifest.json", "data.sqlite"]);
}

#[test]
fn export_fails_when_referenced_cas_file_is_missing() {
    // Item row references a hash whose CAS file got removed —
    // export must fail rather than producing a partial archive.
    // Verifies the load-bearing "no torn output" guarantee.
    let (d, mds) = open();
    let s = fresh_set();
    mds.create_set(&s).unwrap();
    let inbox = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let b = mds.put_blob(b"will be deleted").unwrap();
    mds.add_item(
        &s,
        &b,
        &[Membership {
            container: inbox,
            flags: Flags(0),
            added_at: 1,
        }],
    )
    .unwrap();

    // Manually remove the CAS file out from under the item row.
    let hex = {
        let mut s = String::with_capacity(64);
        for byte in b.0.iter() {
            s.push_str(&format!("{:02x}", byte));
        }
        s
    };
    let cas = d
        .path()
        .join("blobs")
        .join(&hex[0..2])
        .join(&hex[2..4])
        .join(&hex);
    std::fs::remove_file(&cas).unwrap();

    let dest = d.path().join("torn.tar");
    let result = mds.export_set(&s, &dest);
    assert!(result.is_err(), "export should fail: {result:?}");
    assert!(
        !dest.exists(),
        "no partial archive should remain at {}",
        dest.display()
    );
    // The .partial sibling should also be cleaned up.
    let partial = d.path().join("torn.tar.partial");
    assert!(
        !partial.exists(),
        "partial sibling should be cleaned up: {}",
        partial.display()
    );
}

#[test]
fn export_unknown_set_returns_set_not_found() {
    let (d, mds) = open();
    let bogus = fresh_set();
    let dest = d.path().join("nope.tar");
    let err = mds.export_set(&bogus, &dest).unwrap_err();
    assert!(
        matches!(err, cosmix_mds::Error::SetNotFound(_)),
        "got {err:?}"
    );
    assert!(!dest.exists());
}

#[test]
fn export_data_sqlite_is_a_valid_sqlite_database() {
    // The snapshot must round-trip through rusqlite — i.e. it's
    // not a torn copy that just happens to have the right size.
    let (d, mds) = open();
    let s = fresh_set();
    mds.create_set(&s).unwrap();
    let inbox = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let h = mds.put_blob(b"roundtrip").unwrap();
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

    let dest = d.path().join("rt.tar");
    mds.export_set(&s, &dest).unwrap();

    let entries = read_tar_entries(&dest);
    let data = entries
        .iter()
        .find(|(n, _)| n == "data.sqlite")
        .expect("data.sqlite present")
        .1
        .clone();

    // Write to a sibling temp file and open with rusqlite.
    let scratch = d.path().join("verify-data.sqlite");
    std::fs::write(&scratch, &data).unwrap();
    let conn = rusqlite::Connection::open(&scratch).unwrap();
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM item", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1);
    let bc: i64 = conn
        .query_row("SELECT COUNT(*) FROM container", [], |r| r.get(0))
        .unwrap();
    assert_eq!(bc, 1);

    // VACUUM INTO operates on `main` only; the ATTACH'd
    // `blobs_db` (refcount/blob_ref tables) MUST NOT leak into
    // the snapshot. If a future maintainer swaps to a different
    // snapshot mechanism, this assertion catches the regression.
    let tables: Vec<String> = {
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table'")
            .unwrap();
        let rows = stmt.query_map([], |r| r.get::<_, String>(0)).unwrap();
        rows.map(|r| r.unwrap()).collect()
    };
    for forbidden in ["blob", "blob_ref", "blob_verify", "refcount_pending"] {
        assert!(
            !tables.iter().any(|t| t == forbidden),
            "snapshot must not include {forbidden}; tables: {tables:?}"
        );
    }
    // Per-set tables that should be present.
    for required in [
        "item",
        "container",
        "membership",
        "container_change",
        "mail_envelopes",
    ] {
        assert!(
            tables.iter().any(|t| t == required),
            "snapshot missing {required}; tables: {tables:?}"
        );
    }
}
