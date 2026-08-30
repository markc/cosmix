//! Phase 4: bitrot verification ledger.
//!
//! Covers the contract from `_doc/2026-04-29-cosmix-mds-plan.md`
//! § "Phase 4 — verification ledger":
//!
//! - Full-scope sweep over a clean store: every blob recorded with
//!   `status=0` (ok) in `blob_verify`, mismatches=0.
//! - Bytes-corruption detection: overwrite one byte of a CAS file,
//!   verify, assert `status=1` and mismatches incremented.
//! - File-missing detection: unlink the CAS file, verify, assert
//!   `status=2` and mismatches incremented.
//! - `Since(t)` skips blobs whose `last_verified_at >= t` and
//!   re-verifies stale ones (and ones never verified).
//! - `Container(cid)` restricts the sweep to blobs referenced from
//!   that container; blobs in other containers are not touched.
//! - `last_verified_at` advances on subsequent runs (UPSERT shape
//!   is correct — no INSERT-only bug that would leave the ledger
//!   stuck after the first sweep).
//! - Lock ordering: `verify_blobs(Container(_))` must not deadlock
//!   against a concurrent `delete_set` on a different set. The fix
//!   resolves Container hashes via per-set Mutexes *before* taking
//!   `self.blobs`, matching `delete_set`'s set→blobs order.

use cosmix_mds::{
    BlobHash, ContainerAttrs, Flags, Mds, Membership, SetId, SqliteCasMds, VerifyScope,
};
use rusqlite::{Connection, params};
use std::path::Path;
use std::time::{Duration, SystemTime};
use tempfile::TempDir;

fn open() -> (TempDir, SqliteCasMds) {
    let d = TempDir::new().unwrap();
    let m = SqliteCasMds::open(d.path()).unwrap();
    (d, m)
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

fn flags() -> Flags {
    Flags(0)
}

fn hex(h: &BlobHash) -> String {
    let mut s = String::with_capacity(64);
    for b in h.0.iter() {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

fn blobs_db(root: &Path) -> Connection {
    Connection::open(root.join("blobs.sqlite")).unwrap()
}

fn blob_path(root: &Path, h: &BlobHash) -> std::path::PathBuf {
    let h = hex(h);
    root.join("blobs").join(&h[0..2]).join(&h[2..4]).join(h)
}

fn verify_row(c: &Connection, h: &BlobHash) -> Option<(i64, i64)> {
    c.query_row(
        "SELECT last_verified_at, status FROM blob_verify WHERE hash = ?1",
        params![hex(h)],
        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
    )
    .ok()
}

fn add_one(
    mds: &SqliteCasMds,
    set: &SetId,
    container: cosmix_mds::ContainerId,
    body: &[u8],
) -> BlobHash {
    let blob = mds.put_blob(body).unwrap();
    mds.add_item(
        set,
        &blob,
        &[Membership {
            container,
            flags: flags(),
            added_at: 1,
        }],
    )
    .unwrap();
    blob
}

#[test]
fn full_scope_on_empty_store_is_zero() {
    let (_d, mds) = open();
    let report = mds.verify_blobs(VerifyScope::Full).unwrap();
    assert_eq!(report.blobs_checked, 0);
    assert_eq!(report.mismatches, 0);
}

#[test]
fn full_scope_marks_clean_blobs_ok() {
    let (d, mds) = open();
    let s = fresh_set();
    mds.create_set(&s).unwrap();
    let inbox = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let h1 = add_one(&mds, &s, inbox, b"alpha");
    let h2 = add_one(&mds, &s, inbox, b"beta");

    let report = mds.verify_blobs(VerifyScope::Full).unwrap();
    assert_eq!(report.blobs_checked, 2);
    assert_eq!(report.mismatches, 0);

    let bdb = blobs_db(d.path());
    let (_, status1) = verify_row(&bdb, &h1).expect("h1 verify row");
    let (_, status2) = verify_row(&bdb, &h2).expect("h2 verify row");
    assert_eq!(status1, 0);
    assert_eq!(status2, 0);
}

#[test]
fn detects_byte_corruption_as_status_1() {
    let (d, mds) = open();
    let s = fresh_set();
    mds.create_set(&s).unwrap();
    let inbox = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let h = add_one(&mds, &s, inbox, b"this is the original payload");

    // Overwrite one byte of the on-disk CAS file. Same length, same
    // path, just wrong content — exactly the bitrot shape verify
    // exists for.
    let path = blob_path(d.path(), &h);
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[0] ^= 0x01;
    std::fs::write(&path, &bytes).unwrap();

    let report = mds.verify_blobs(VerifyScope::Full).unwrap();
    assert_eq!(report.blobs_checked, 1);
    assert_eq!(report.mismatches, 1);

    let bdb = blobs_db(d.path());
    let (_, status) = verify_row(&bdb, &h).expect("verify row written");
    assert_eq!(status, 1, "expected hash-mismatch status");
}

#[test]
fn detects_missing_file_as_status_2() {
    let (d, mds) = open();
    let s = fresh_set();
    mds.create_set(&s).unwrap();
    let inbox = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let h = add_one(&mds, &s, inbox, b"will be unlinked");
    let path = blob_path(d.path(), &h);
    std::fs::remove_file(&path).unwrap();

    let report = mds.verify_blobs(VerifyScope::Full).unwrap();
    assert_eq!(report.blobs_checked, 1);
    assert_eq!(report.mismatches, 1);

    let bdb = blobs_db(d.path());
    let (_, status) = verify_row(&bdb, &h).expect("verify row written");
    assert_eq!(status, 2, "expected missing-file status");
}

#[test]
fn since_skips_recently_verified_blobs() {
    let (d, mds) = open();
    let s = fresh_set();
    mds.create_set(&s).unwrap();
    let inbox = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let h_old = add_one(&mds, &s, inbox, b"old");

    // Capture the cutoff *before* the first verify so h_old's
    // last_verified_at lands strictly after it. Operator semantics:
    // `Since(t)` re-verifies anything not verified since `t`, so
    // h_old (verified after `t`) must be skipped.
    let cutoff = SystemTime::now();
    std::thread::sleep(Duration::from_millis(10));

    let r1 = mds.verify_blobs(VerifyScope::Full).unwrap();
    assert_eq!(r1.blobs_checked, 1);

    // h_new has no blob_verify row at all; Since must pick it up.
    let h_new = add_one(&mds, &s, inbox, b"new");

    let r2 = mds.verify_blobs(VerifyScope::Since(cutoff)).unwrap();
    assert_eq!(
        r2.blobs_checked, 1,
        "Since(t) should skip h_old (verified after cutoff) and check only h_new"
    );
    assert_eq!(r2.mismatches, 0);

    let bdb = blobs_db(d.path());
    assert!(verify_row(&bdb, &h_old).is_some());
    assert!(verify_row(&bdb, &h_new).is_some());
}

#[test]
fn since_re_verifies_stale_blobs() {
    let (d, mds) = open();
    let s = fresh_set();
    mds.create_set(&s).unwrap();
    let inbox = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let h = add_one(&mds, &s, inbox, b"will go stale");

    mds.verify_blobs(VerifyScope::Full).unwrap();

    // Backdate the verify row to 1970 so any sane Since(t) considers
    // it stale.
    let bdb = blobs_db(d.path());
    bdb.execute(
        "UPDATE blob_verify SET last_verified_at = 0 WHERE hash = ?1",
        params![hex(&h)],
    )
    .unwrap();
    drop(bdb);

    let cutoff = SystemTime::now();
    let r = mds.verify_blobs(VerifyScope::Since(cutoff)).unwrap();
    assert_eq!(r.blobs_checked, 1, "stale blob should be re-checked");

    let bdb = blobs_db(d.path());
    let (last_ms, _) = verify_row(&bdb, &h).expect("verify row");
    assert!(
        last_ms > 0,
        "last_verified_at not advanced by Since(t) — UPSERT bug?"
    );
}

#[test]
fn container_scope_restricts_to_referenced_blobs() {
    let (d, mds) = open();
    let s = fresh_set();
    mds.create_set(&s).unwrap();
    let a = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let b = mds.create_container(&s, None, "Sent", attrs()).unwrap();

    let h_in_a = add_one(&mds, &s, a, b"only in a");
    let h_in_b = add_one(&mds, &s, b, b"only in b");

    let r = mds.verify_blobs(VerifyScope::Container(a)).unwrap();
    assert_eq!(r.blobs_checked, 1);
    assert_eq!(r.mismatches, 0);

    let bdb = blobs_db(d.path());
    assert!(
        verify_row(&bdb, &h_in_a).is_some(),
        "container A blob should have a verify row"
    );
    assert!(
        verify_row(&bdb, &h_in_b).is_none(),
        "container B blob must not be touched by Container(a) scope"
    );
}

#[test]
fn container_scope_does_not_deadlock_against_concurrent_delete_set() {
    // Regression test for the lock-order inversion Codex caught in
    // the first Phase 4 implementation. `delete_set` takes the
    // per-set Mutex then `self.blobs`. The original
    // `verify_blobs(Container)` resolved hashes via a closure run
    // *inside* the blobs.lock() critical section, which inverted
    // the order to blobs→set and produced an AB-BA deadlock against
    // a concurrent `delete_set` on any set. The fix resolves the
    // candidate hash list before taking blobs.lock(), keeping
    // set→blobs across the crate.
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    let d = TempDir::new().unwrap();
    let mds = Arc::new(SqliteCasMds::open(d.path()).unwrap());

    // Long-lived set whose container is the verify target.
    let a = fresh_set();
    mds.create_set(&a).unwrap();
    let inbox = mds.create_container(&a, None, "INBOX", attrs()).unwrap();
    add_one(&mds, &a, inbox, b"target");

    let stop = Arc::new(AtomicBool::new(false));
    let v_count = Arc::new(AtomicU64::new(0));
    let d_count = Arc::new(AtomicU64::new(0));

    let v_handle = {
        let mds = Arc::clone(&mds);
        let stop = Arc::clone(&stop);
        let count = Arc::clone(&v_count);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                mds.verify_blobs(VerifyScope::Container(inbox)).unwrap();
                count.fetch_add(1, Ordering::Relaxed);
            }
        })
    };

    let d_handle = {
        let mds = Arc::clone(&mds);
        let stop = Arc::clone(&stop);
        let count = Arc::clone(&d_count);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                // Each iteration creates+deletes a throwaway set,
                // exercising the (per-set Mutex → self.blobs) path
                // that races verify's order discipline.
                let s = fresh_set();
                mds.create_set(&s).unwrap();
                mds.delete_set(&s).unwrap();
                count.fetch_add(1, Ordering::Relaxed);
            }
        })
    };

    std::thread::sleep(Duration::from_millis(500));
    stop.store(true, Ordering::Relaxed);

    v_handle.join().expect("verify thread panicked");
    d_handle.join().expect("delete thread panicked");

    let vc = v_count.load(Ordering::Relaxed);
    let dc = d_count.load(Ordering::Relaxed);
    // Both counters > 0 confirms forward progress; the join above
    // is the deadlock backstop. With the inverted order, the test
    // would block on join (cargo's harness timeout fires).
    assert!(vc > 0, "verify thread made no progress (vc={vc})");
    assert!(dc > 0, "delete thread made no progress (dc={dc})");
}

#[test]
fn upsert_advances_last_verified_at_on_repeat_sweeps() {
    let (d, mds) = open();
    let s = fresh_set();
    mds.create_set(&s).unwrap();
    let inbox = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let h = add_one(&mds, &s, inbox, b"sticky");

    mds.verify_blobs(VerifyScope::Full).unwrap();
    let bdb = blobs_db(d.path());
    let (first, _) = verify_row(&bdb, &h).unwrap();
    drop(bdb);

    std::thread::sleep(Duration::from_millis(5));
    mds.verify_blobs(VerifyScope::Full).unwrap();
    let bdb = blobs_db(d.path());
    let (second, _) = verify_row(&bdb, &h).unwrap();
    assert!(
        second >= first,
        "last_verified_at must not regress; first={first}, second={second}"
    );
    assert!(
        second > first,
        "second sweep should advance last_verified_at; UPSERT may be missing"
    );
}
