//! Integration tests for `Mds::import_set`.
//!
//! Round-trip via `export_set` covers the happy path (the produced
//! archive must import cleanly into a fresh root and the imported
//! set must be queryable end-to-end). The remainder pin the
//! defensive contract from the spec: reject path traversal,
//! reject absolute paths, reject duplicates, reject manifest /
//! data mismatches, reject blob name/content disagreement, reject
//! a set already present. Each rejection test crafts a tar by
//! hand so the failure modes are exercised even when the exporter
//! itself would never produce them.

use cosmix_mds::{ContainerAttrs, Error, Flags, Mds, Membership, SetId, SqliteCasMds};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::Duration;
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

/// Build an exporter root with one set + one item, return the
/// produced tarball path (and the source TempDir, kept alive by the
/// caller). Uses `export_set` so the tarball is a real, valid
/// artifact for round-trip tests.
fn export_one_item_set() -> (TempDir, PathBuf, SetId, [u8; 32]) {
    let (d, mds) = open();
    let s = fresh_set();
    mds.create_set(&s).unwrap();
    let inbox = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let h = mds.put_blob(b"round-trip me").unwrap();
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
    let dest = d.path().join("out.tar");
    mds.export_set(&s, &dest).unwrap();
    drop(mds);
    (d, dest, s, h.0)
}

#[test]
fn round_trip_export_then_import_yields_queryable_set() {
    let (_src, tarball, s, expected_hash) = export_one_item_set();
    let (_d, dst) = open();

    let report = dst.import_set(&tarball).unwrap();
    assert_eq!(report.set_id, s);
    assert_eq!(report.item_count, 1);
    assert_eq!(report.blob_count, 1);
    assert!(report.bytes_read > 0);

    // Set is visible.
    let sets = dst.list_sets().unwrap();
    assert!(sets.contains(&s));

    // Container + item visible.
    let containers = dst.list_containers(&s).unwrap();
    assert_eq!(containers.len(), 1);
    let inbox = containers[0].id;
    let items = dst
        .list_items(&s, &inbox, cosmix_mds::SeqRange::All)
        .unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].meta.blob_hash.0, expected_hash);

    // Blob is in CAS — fetch the bytes back through the public API.
    let bytes = dst.get_blob(&cosmix_mds::BlobHash(expected_hash)).unwrap();
    assert_eq!(bytes, b"round-trip me".to_vec());
}

#[test]
fn imported_connection_keeps_the_store_busy_timeout() {
    let (_src, tarball, set, _hash) = export_one_item_set();
    let root = TempDir::new().unwrap();
    let wait = Duration::from_millis(37);
    let dst = SqliteCasMds::open_with_busy_timeout(root.path(), wait).unwrap();

    dst.import_set(&tarball).unwrap();
    let observed: i64 = dst
        .with_set_tx(&set, |handle| {
            handle
                .tx()
                .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
                .map_err(|e| Error::Other(format!("read imported busy_timeout: {e}")))
        })
        .unwrap();

    assert_eq!(observed, wait.as_millis() as i64);
}

#[test]
fn import_rejects_set_already_present() {
    let (_src, tarball, _s, _h) = export_one_item_set();
    let (_d, dst) = open();
    dst.import_set(&tarball).unwrap();
    let err = dst.import_set(&tarball).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("already exists"), "got {msg}");
}

#[test]
fn import_with_missing_tarball_errors() {
    let (_d, dst) = open();
    let err = dst
        .import_set(Path::new("/does/not/exist/xyz.tar"))
        .unwrap_err();
    assert!(matches!(err, Error::Io(_)), "got {err:?}");
}

// ---- Hand-crafted tar fixtures for rejection cases ----

/// Build a single 512-byte ustar header for `name` + `size`.
/// We write the bytes ourselves rather than going through
/// `tar::Builder::append_data`, which rejects `..` and absolute
/// paths at write time — we *want* those entries in the archive
/// so the import path's defenses can be exercised.
fn ustar_header(name: &str, size: u64) -> [u8; 512] {
    let mut hdr = [0u8; 512];
    let nb = name.as_bytes();
    assert!(nb.len() < 100, "test names must fit ustar `name` field");
    hdr[..nb.len()].copy_from_slice(nb);
    // mode "0000644\0"
    hdr[100..108].copy_from_slice(b"0000644\0");
    // uid / gid "0000000\0" (default zeros + null)
    hdr[108..116].copy_from_slice(b"0000000\0");
    hdr[116..124].copy_from_slice(b"0000000\0");
    // size (12 bytes, octal, null-terminated to 11 + null)
    let size_str = format!("{:011o}\0", size);
    hdr[124..136].copy_from_slice(size_str.as_bytes());
    // mtime "00000000000\0"
    hdr[136..148].copy_from_slice(b"00000000000\0");
    // cksum: filled with spaces during checksum, then overwritten.
    hdr[148..156].copy_from_slice(b"        ");
    // typeflag '0' = regular file
    hdr[156] = b'0';
    // magic + version: ustar\000
    hdr[257..263].copy_from_slice(b"ustar\0");
    hdr[263..265].copy_from_slice(b"00");
    // checksum = sum of all bytes (with cksum field as spaces)
    let cksum: u32 = hdr.iter().map(|&b| b as u32).sum();
    let cksum_str = format!("{:06o}\0 ", cksum);
    hdr[148..156].copy_from_slice(cksum_str.as_bytes());
    hdr
}

fn write_entry_raw<W: std::io::Write>(w: &mut W, name: &str, bytes: &[u8]) {
    let hdr = ustar_header(name, bytes.len() as u64);
    w.write_all(&hdr).unwrap();
    w.write_all(bytes).unwrap();
    // Pad data to 512-byte boundary.
    let pad = (512 - (bytes.len() % 512)) % 512;
    if pad > 0 {
        w.write_all(&vec![0u8; pad]).unwrap();
    }
}

fn manifest_bytes(set_uuid: uuid::Uuid, item_count: u64, blob_count: u64) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "format": "cosmix-mds-set-export",
        "format_version": 1,
        "set_id": set_uuid.to_string(),
        // Matches a fresh export's user_version (DATA_LATEST, now v9).
        "schema_version": 9,
        "exported_at": "2026-05-03T00:00:00Z",
        "item_count": item_count,
        "blob_count": blob_count,
    }))
    .unwrap()
}

/// Empty, schema-valid `data.sqlite` payload for fixture tarballs.
/// Built by opening a temp DB, applying the per-set migrations, and
/// reading the file bytes back — this matches what
/// `apply_data_migrations` produces during import finalize, so the
/// fixture round-trips through `verify_staged_data`'s checks
/// (item count = 0, distinct blobs = 0).
fn empty_data_sqlite_bytes() -> Vec<u8> {
    let d = TempDir::new().unwrap();
    let p = d.path().join("data.sqlite");
    {
        let mut conn = rusqlite::Connection::open(&p).unwrap();
        cosmix_mds::schema::apply_data_migrations(&mut conn).unwrap();
    }
    std::fs::read(&p).unwrap()
}

/// Build a v1-only `data.sqlite` payload — applies *only* the v1
/// migration step (bypassing the migration runner) so the resulting
/// file has `user_version = 1`. Used by the legacy-tarball
/// compatibility test: the import path is required to upgrade
/// such a DB in place via `apply_data_migrations` after rename,
/// per the Phase 5 export/import stability commitment.
fn empty_v1_data_sqlite_bytes() -> Vec<u8> {
    const V1_SQL: &str = include_str!("../src/schema/v1.sql");
    let d = TempDir::new().unwrap();
    let p = d.path().join("data.sqlite");
    {
        let conn = rusqlite::Connection::open(&p).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;\
             PRAGMA synchronous  = NORMAL;\
             PRAGMA foreign_keys = ON;",
        )
        .unwrap();
        conn.execute_batch(V1_SQL).unwrap();
        conn.pragma_update(None, "user_version", 1u32).unwrap();
        conn.pragma_update(None, "application_id", 0x636D_6473i32)
            .unwrap();
    }
    std::fs::read(&p).unwrap()
}

/// Build a v1.1-only `data.sqlite` payload — applies the v1 + v1_1
/// migration steps and stops at `user_version = 2`. Used by the v2
/// legacy-tarball test: a tarball produced by a build at
/// `DATA_LATEST = 2` (the state shipped before the v1.2 mail_envelopes
/// migration landed) must still import on this build, with
/// `apply_data_migrations` stepping the per-set DB from 2 → 3 in
/// place after rename. This is the realistic upgrade path for any
/// existing deployment that exported its data before this commit.
fn empty_v1_1_data_sqlite_bytes() -> Vec<u8> {
    const V1_SQL: &str = include_str!("../src/schema/v1.sql");
    const V1_1_SQL: &str = include_str!("../src/schema/v1_1.sql");
    let d = TempDir::new().unwrap();
    let p = d.path().join("data.sqlite");
    {
        let conn = rusqlite::Connection::open(&p).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;\
             PRAGMA synchronous  = NORMAL;\
             PRAGMA foreign_keys = ON;",
        )
        .unwrap();
        conn.execute_batch(V1_SQL).unwrap();
        conn.execute_batch(V1_1_SQL).unwrap();
        // v1_1.sql sets `PRAGMA user_version = 2` inline, but mirror
        // `empty_v1_data_sqlite_bytes`'s belt-and-suspenders pattern
        // and re-assert it explicitly: rusqlite's batch handling can
        // wrap multi-statement input in an implicit transaction in
        // some configurations, where inline PRAGMA is silently
        // dropped (see the comment in `schema::apply_data_migrations`).
        conn.pragma_update(None, "user_version", 2u32).unwrap();
        conn.pragma_update(None, "application_id", 0x636D_6473i32)
            .unwrap();
    }
    std::fs::read(&p).unwrap()
}

/// Build a manifest with an arbitrary `schema_version`. The default
/// `manifest_bytes` helper hard-codes the latest version; this one
/// is used by the legacy-compat test to produce a v1-claiming
/// manifest paired with a v1 `data.sqlite`.
fn manifest_bytes_with_version(
    set_uuid: uuid::Uuid,
    item_count: u64,
    blob_count: u64,
    schema_version: u32,
) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "format": "cosmix-mds-set-export",
        "format_version": 1,
        "set_id": set_uuid.to_string(),
        "schema_version": schema_version,
        "exported_at": "2026-05-03T00:00:00Z",
        "item_count": item_count,
        "blob_count": blob_count,
    }))
    .unwrap()
}

fn build_tar(path: &Path, entries: &[(String, Vec<u8>)]) {
    use std::io::Write;
    let mut f = File::create(path).unwrap();
    for (name, bytes) in entries {
        write_entry_raw(&mut f, name, bytes);
    }
    // Two 512-byte zero blocks = end-of-archive marker.
    f.write_all(&[0u8; 1024]).unwrap();
    f.sync_all().unwrap();
}

#[test]
fn import_rejects_path_traversal() {
    let (_d, dst) = open();
    let s = fresh_set();
    let tar_path = _d.path().join("traversal.tar");
    build_tar(
        &tar_path,
        &[
            (String::from("manifest.json"), manifest_bytes(s.0, 0, 0)),
            (String::from("../etc/passwd"), b"x".to_vec()),
        ],
    );
    let err = dst.import_set(&tar_path).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("path traversal") || msg.contains("traversal"),
        "got {msg}"
    );
}

#[test]
fn import_rejects_absolute_paths() {
    let (_d, dst) = open();
    let s = fresh_set();
    let tar_path = _d.path().join("absolute.tar");
    build_tar(
        &tar_path,
        &[
            (String::from("manifest.json"), manifest_bytes(s.0, 0, 0)),
            (String::from("/etc/passwd"), b"x".to_vec()),
        ],
    );
    let err = dst.import_set(&tar_path).unwrap_err();
    assert!(format!("{err}").contains("absolute"), "got {err}");
}

#[test]
fn import_rejects_missing_manifest() {
    let (_d, dst) = open();
    let tar_path = _d.path().join("no-manifest.tar");
    build_tar(
        &tar_path,
        &[(String::from("data.sqlite"), empty_data_sqlite_bytes())],
    );
    let err = dst.import_set(&tar_path).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("expected first entry") || msg.contains("manifest"),
        "got {msg}"
    );
}

#[test]
fn import_rejects_duplicate_manifest() {
    let (_d, dst) = open();
    let s = fresh_set();
    let tar_path = _d.path().join("dup-manifest.tar");
    build_tar(
        &tar_path,
        &[
            (String::from("manifest.json"), manifest_bytes(s.0, 0, 0)),
            (String::from("manifest.json"), manifest_bytes(s.0, 0, 0)),
        ],
    );
    let err = dst.import_set(&tar_path).unwrap_err();
    assert!(format!("{err}").contains("duplicate manifest"), "got {err}");
}

#[test]
fn import_rejects_duplicate_data() {
    let (_d, dst) = open();
    let s = fresh_set();
    let tar_path = _d.path().join("dup-data.tar");
    build_tar(
        &tar_path,
        &[
            (String::from("manifest.json"), manifest_bytes(s.0, 0, 0)),
            (String::from("data.sqlite"), empty_data_sqlite_bytes()),
            (String::from("data.sqlite"), empty_data_sqlite_bytes()),
        ],
    );
    let err = dst.import_set(&tar_path).unwrap_err();
    assert!(
        format!("{err}").contains("duplicate data.sqlite"),
        "got {err}"
    );
}

#[test]
fn import_rejects_blob_path_content_mismatch() {
    let (_d, dst) = open();
    let s = fresh_set();
    let tar_path = _d.path().join("bad-blob.tar");
    // Pretend bytes "alpha" go at a hash-named path that actually
    // belongs to "bravo". Pick a deterministic wrong hex.
    let wrong_hex = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
    let entry_name = format!(
        "blobs/{}/{}/{}",
        &wrong_hex[0..2],
        &wrong_hex[2..4],
        wrong_hex
    );
    build_tar(
        &tar_path,
        &[
            (String::from("manifest.json"), manifest_bytes(s.0, 0, 1)),
            (String::from("data.sqlite"), empty_data_sqlite_bytes()),
            (entry_name, b"alpha".to_vec()),
        ],
    );
    let err = dst.import_set(&tar_path).unwrap_err();
    assert!(format!("{err}").contains("hash mismatch"), "got {err}");
}

#[test]
fn import_rejects_unknown_root_entry() {
    let (_d, dst) = open();
    let s = fresh_set();
    let tar_path = _d.path().join("bogus-root.tar");
    build_tar(
        &tar_path,
        &[
            (String::from("manifest.json"), manifest_bytes(s.0, 0, 0)),
            (String::from("README"), b"hi".to_vec()),
        ],
    );
    let err = dst.import_set(&tar_path).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("unexpected tar entry") || msg.contains("README"),
        "got {msg}"
    );
}

#[test]
fn import_rejects_wrong_format_tag() {
    let (_d, dst) = open();
    let s = fresh_set();
    let tar_path = _d.path().join("wrong-tag.tar");
    let bad_manifest = serde_json::to_vec(&serde_json::json!({
        "format": "not-our-export",
        "format_version": 1,
        "set_id": s.0.to_string(),
        "schema_version": 4,
        "exported_at": "2026-05-03T00:00:00Z",
        "item_count": 0,
        "blob_count": 0,
    }))
    .unwrap();
    build_tar(
        &tar_path,
        &[
            (String::from("manifest.json"), bad_manifest),
            (String::from("data.sqlite"), empty_data_sqlite_bytes()),
        ],
    );
    let err = dst.import_set(&tar_path).unwrap_err();
    assert!(format!("{err}").contains("manifest.format"), "got {err}");
}

#[test]
fn import_rejects_wrong_format_version() {
    let (_d, dst) = open();
    let s = fresh_set();
    let tar_path = _d.path().join("wrong-version.tar");
    let bad_manifest = serde_json::to_vec(&serde_json::json!({
        "format": "cosmix-mds-set-export",
        "format_version": 99,
        "set_id": s.0.to_string(),
        "schema_version": 4,
        "exported_at": "2026-05-03T00:00:00Z",
        "item_count": 0,
        "blob_count": 0,
    }))
    .unwrap();
    build_tar(
        &tar_path,
        &[
            (String::from("manifest.json"), bad_manifest),
            (String::from("data.sqlite"), empty_data_sqlite_bytes()),
        ],
    );
    let err = dst.import_set(&tar_path).unwrap_err();
    assert!(format!("{err}").contains("format_version"), "got {err}");
}

/// Phase 5 export/import is a v1 stability commitment: a tarball
/// produced by an older build (manifest.schema_version = 1, data.sqlite
/// at user_version = 1) must still import on this build, with the
/// per-set DB upgraded in place to the current schema. The upgrade
/// hook is the existing `apply_data_migrations` call in
/// `import::finalize`, which now lands at `DATA_LATEST = 5` and runs
/// `seed_set_state` immediately afterwards.
#[test]
fn import_accepts_legacy_v1_tarball_and_upgrades_to_latest() {
    let (root, dst) = open();
    let s = fresh_set();
    let tar_path = root.path().join("legacy-v1.tar");
    build_tar(
        &tar_path,
        &[
            (
                String::from("manifest.json"),
                manifest_bytes_with_version(s.0, 0, 0, 1),
            ),
            (String::from("data.sqlite"), empty_v1_data_sqlite_bytes()),
        ],
    );
    dst.import_set(&tar_path)
        .expect("legacy v1 tarball must import");

    // The per-set DB was upgraded in place by apply_data_migrations
    // during finalize. Open it fresh (read-only) and confirm.
    let data_path = root
        .path()
        .join("containers")
        .join(s.0.to_string())
        .join("data.sqlite");
    let conn = rusqlite::Connection::open(&data_path).unwrap();
    let v: u32 = conn
        .query_row("PRAGMA user_version;", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v, 9, "legacy v1 import must auto-upgrade to DATA_LATEST");
    // v1.1 / v1.2 tables landed.
    for tbl in [
        "set_state",
        "set_change",
        "blob_refs",
        "mail_threads",
        "mail_envelopes",
    ] {
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name=?1;",
                [tbl],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "v1.1 table {tbl} missing after legacy import");
    }
    // seed_set_state ran during finalize: the singleton row is
    // present and the allocator counter starts at 0.
    let (id, seq): (String, i64) = conn
        .query_row("SELECT set_id, set_change_seq FROM set_state;", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(id, s.0.to_string());
    assert_eq!(seq, 0);
}

/// Pin the v2 → v3 upgrade path: a tarball whose `data.sqlite` is at
/// `user_version = 2` and whose manifest claims `schema_version = 2`
/// (the realistic shape for any existing deployment that exported
/// before v1.2 landed) must import and auto-upgrade in place. Without
/// this test, the v2 → v3 branch of `apply_data_migrations` is
/// untested even though it is the load-bearing real-world path.
#[test]
fn import_accepts_v1_1_tarball_and_upgrades_to_latest() {
    let (root, dst) = open();
    let s = fresh_set();
    let tar_path = root.path().join("legacy-v1_1.tar");
    build_tar(
        &tar_path,
        &[
            (
                String::from("manifest.json"),
                manifest_bytes_with_version(s.0, 0, 0, 2),
            ),
            (String::from("data.sqlite"), empty_v1_1_data_sqlite_bytes()),
        ],
    );
    dst.import_set(&tar_path)
        .expect("v1.1 (schema_version=2) tarball must import");

    let data_path = root
        .path()
        .join("containers")
        .join(s.0.to_string())
        .join("data.sqlite");
    let conn = rusqlite::Connection::open(&data_path).unwrap();
    let v: u32 = conn
        .query_row("PRAGMA user_version;", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v, 9, "v1.1 import must auto-upgrade to DATA_LATEST");
    // mail_envelopes (only v1.2 table) must be present after the
    // 2 → 3 step ran.
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name='mail_envelopes';",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 1, "mail_envelopes table missing after v1.1 import");
}

#[test]
fn import_rejects_wrong_schema_version() {
    let (_d, dst) = open();
    let s = fresh_set();
    let tar_path = _d.path().join("wrong-schema.tar");
    let bad_manifest = serde_json::to_vec(&serde_json::json!({
        "format": "cosmix-mds-set-export",
        "format_version": 1,
        "set_id": s.0.to_string(),
        "schema_version": 99,
        "exported_at": "2026-05-03T00:00:00Z",
        "item_count": 0,
        "blob_count": 0,
    }))
    .unwrap();
    build_tar(
        &tar_path,
        &[
            (String::from("manifest.json"), bad_manifest),
            (String::from("data.sqlite"), empty_data_sqlite_bytes()),
        ],
    );
    let err = dst.import_set(&tar_path).unwrap_err();
    assert!(format!("{err}").contains("schema_version"), "got {err}");
}

#[test]
fn import_rejects_blob_count_mismatch() {
    let (_d, dst) = open();
    let s = fresh_set();
    let tar_path = _d.path().join("count-mismatch.tar");
    // Manifest says blob_count=1, but no blob entry follows.
    build_tar(
        &tar_path,
        &[
            (String::from("manifest.json"), manifest_bytes(s.0, 0, 1)),
            (String::from("data.sqlite"), empty_data_sqlite_bytes()),
        ],
    );
    let err = dst.import_set(&tar_path).unwrap_err();
    assert!(
        format!("{err}").contains("manifest.blob_count"),
        "got {err}"
    );
}

#[test]
fn import_rejects_data_blob_set_disagreement() {
    // Craft an archive where data.sqlite references hash(alpha) but
    // the tar ships only a blob for hash(bravo) — same count (1),
    // disjoint hash sets. Without the cross-check this would import
    // successfully and leave the set queryable with the alpha bytes
    // missing from CAS.
    //
    // Build the data.sqlite by going through a real exporter so it
    // round-trips through `verify_staged_data`'s schema/count gates;
    // then repackage with a wrong-content blob entry.
    let src_dir = TempDir::new().unwrap();
    let src = SqliteCasMds::open(src_dir.path()).unwrap();
    let s = fresh_set();
    src.create_set(&s).unwrap();
    let inbox = src.create_container(&s, None, "INBOX", attrs()).unwrap();
    let alpha = src.put_blob(b"alpha").unwrap();
    src.add_item(
        &s,
        &alpha,
        &[Membership {
            container: inbox,
            flags: Flags(0),
            added_at: 1,
        }],
    )
    .unwrap();
    drop(src);

    // Read the on-disk per-set data.sqlite directly.
    let data_path = src_dir
        .path()
        .join("containers")
        .join(s.0.to_string())
        .join("data.sqlite");
    let data_bytes = std::fs::read(&data_path).unwrap();

    // Hash unrelated content "bravo" — disjoint from "alpha".
    let bravo_hash = cosmix_mds::blob::hash_bytes(b"bravo");
    let bravo_hex: String = bravo_hash.0.iter().map(|b| format!("{:02x}", b)).collect();
    let entry_name = format!(
        "blobs/{}/{}/{}",
        &bravo_hex[0..2],
        &bravo_hex[2..4],
        bravo_hex
    );

    let (_d, dst) = open();
    let tar_path = _d.path().join("disagree.tar");
    build_tar(
        &tar_path,
        &[
            (String::from("manifest.json"), manifest_bytes(s.0, 1, 1)),
            (String::from("data.sqlite"), data_bytes),
            (entry_name, b"bravo".to_vec()),
        ],
    );

    let err = dst.import_set(&tar_path).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("blob_hash set disagrees"), "got {msg}");

    // Belt-and-suspenders for the Medium fix: the destination must
    // remain pristine — no `containers/<uuid>/` should be left behind
    // for a later open() to discover as a real set.
    let target = _d.path().join("containers").join(s.0.to_string());
    assert!(
        !target.exists(),
        "post-rejection target dir should not exist"
    );
}

#[test]
fn import_round_trip_blob_ref_refcount_holds() {
    // After import, the box-wide blobs.sqlite should have a
    // refcount==1 row for the imported blob and a single blob_ref
    // pointing at the imported set. This is the load-bearing
    // refcount-drift guarantee — without per-set ingest in finalize,
    // gc could collect a still-referenced blob.
    let (_src, tarball, s, hash) = export_one_item_set();
    let (d, dst) = open();
    dst.import_set(&tarball).unwrap();

    let blobs_path = d.path().join("blobs.sqlite");
    let conn = rusqlite::Connection::open(&blobs_path).unwrap();
    let hex: String = {
        let mut out = String::with_capacity(64);
        for b in hash.iter() {
            out.push_str(&format!("{:02x}", b));
        }
        out
    };
    let refcount: i64 = conn
        .query_row(
            "SELECT refcount FROM blob WHERE hash = ?",
            rusqlite::params![hex],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(refcount, 1, "imported blob refcount should be 1");

    let ref_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM blob_ref WHERE hash = ? AND set_id = ?",
            rusqlite::params![hex, s.0.to_string()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        ref_count, 1,
        "blob_ref should reference the imported set exactly once"
    );
}
