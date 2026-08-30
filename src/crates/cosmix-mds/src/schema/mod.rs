//! Schema migration runner.
//!
//! Applies versioned `vN.sql` files to a SQLite connection in order,
//! using `PRAGMA user_version` as the durable marker. PRAGMA
//! application_id is set inside each `vN.sql` so a fresh DB carries
//! the right magic number, and a wrong-magic DB is detected on open.
//!
//! Two flavors:
//!
//! - `apply_data_migrations` — for per-set `data.sqlite`. Magic =
//!   0x636D6473 ('cmds').
//! - `apply_blobs_migrations` — for box-wide `blobs.sqlite`. Magic =
//!   0x62696478 ('bidx').
//!
//! Both run inside a single transaction per migration step, so a
//! crash mid-migration leaves the DB at the prior version.

use crate::error::{Error, Result};
use crate::types::SetId;
use rusqlite::Connection;

const DATA_APPLICATION_ID: i32 = 0x636D_6473; // 'cmds'
const BLOBS_APPLICATION_ID: i32 = 0x6269_6478; // 'bidx'

const DATA_V1_SQL: &str = include_str!("v1.sql");
const DATA_V1_1_SQL: &str = include_str!("v1_1.sql");
const DATA_V1_2_SQL: &str = include_str!("v1_2.sql");
const DATA_V1_3_SQL: &str = include_str!("v1_3.sql");
const DATA_V1_4_SQL: &str = include_str!("v1_4.sql");
const DATA_V1_5_SQL: &str = include_str!("v1_5.sql");
const DATA_V1_6_SQL: &str = include_str!("v1_6.sql");
const DATA_V1_7_SQL: &str = include_str!("v1_7.sql");
const DATA_V1_8_SQL: &str = include_str!("v1_8.sql");
const BLOBS_V1_SQL: &str = include_str!("blobs_v1.sql");

/// Latest per-set `data.sqlite` schema version.
///
/// History:
///   v2 — v1.1 amendment (set_state, set_change, blob_refs,
///        mail_retrain_outbox, mail_threads, mail_search).
///   v3 — v1.2 (mail_envelopes; JMAP→MDS migration Phase 1 Task 1.6/1.7).
///   v4 — v1.3 (mail_envelopes.cc/bcc/reply_to; JMAP→MDS migration
///        Phase 5 Task 5.4a — closes substrate gap C6 for envelope
///        recipient lists used by EmailSubmission auto-detect).
///   v5 — v1.4 (container_change_set; mailbox-changes-substrate Phase 1
///        — writes only, JMAP read path cuts over in Phase 2).
///   v6 — v1.5 (set_state.{container_change_set_floor, set_change_floor};
///        mailbox-changes-substrate Phase 3 — per-stream retention
///        watermarks for `cannotCalculateChanges` rejection).
///   v7 — v1.6 (mail_search DROP + re-CREATE with
///        contentless_unindexed=1, contentless_delete=1; maild Phase
///        8e gate 2 — synchronous FTS projection writes).
///   v8 — v1.7 (mail_envelopes.normalized_subject + index; maild
///        Phase 8e gate 4 — JMAP §4.1.5 / RFC 5256 §2.1 subject-
///        fallback threading after In-Reply-To / References miss).
///   v9 — v1.8 (mail_retrain_outbox + mail_threads gain item(id)
///        ON DELETE CASCADE via table-rebuild; mail_search reaped by
///        an AFTER DELETE ON item trigger — closes the maild
///        item-keyed sidecar reap gap on the last-membership item
///        delete path in container.rs).
const DATA_LATEST: u32 = 9;
const BLOBS_LATEST: u32 = 1;

pub fn apply_data_migrations(conn: &mut Connection) -> Result<()> {
    set_pragmas(conn)?;
    apply(
        conn,
        "data",
        DATA_APPLICATION_ID,
        DATA_LATEST,
        |v| match v {
            1 => Some(DATA_V1_SQL),
            2 => Some(DATA_V1_1_SQL),
            3 => Some(DATA_V1_2_SQL),
            4 => Some(DATA_V1_3_SQL),
            5 => Some(DATA_V1_4_SQL),
            6 => Some(DATA_V1_5_SQL),
            7 => Some(DATA_V1_6_SQL),
            8 => Some(DATA_V1_7_SQL),
            9 => Some(DATA_V1_8_SQL),
            _ => None,
        },
    )
}

/// Ensure the singleton `set_state` row exists for `set`. Idempotent;
/// safe to call after every `apply_data_migrations` on a per-set
/// `data.sqlite`. Must be called by every direct opener of a per-set
/// connection — currently `store::open_set_db` and `import::finalize`.
///
/// The `set_change_seq` allocator (`UPDATE set_state SET
/// set_change_seq = set_change_seq + 1 ... RETURNING`) also performs
/// `INSERT OR IGNORE` defensively so a missed open path self-heals
/// on first mutation; this helper is the normal hot path that keeps
/// the row materialised at open time.
pub fn seed_set_state(conn: &Connection, set: &SetId) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO set_state (set_id, set_change_seq) VALUES (?1, 0);",
        [set.0.to_string()],
    )
    .map_err(|e| Error::SchemaMigration(format!("seed set_state for {}: {e}", set.0)))?;
    Ok(())
}

pub fn apply_blobs_migrations(conn: &mut Connection) -> Result<()> {
    set_pragmas(conn)?;
    apply(
        conn,
        "blobs",
        BLOBS_APPLICATION_ID,
        BLOBS_LATEST,
        |v| match v {
            1 => Some(BLOBS_V1_SQL),
            _ => None,
        },
    )
}

fn set_pragmas(conn: &Connection) -> Result<()> {
    // WAL + NORMAL sync: spec §Account/container schema and §Blob-index schema.
    // Foreign keys must be ON to make ON DELETE CASCADE / RESTRICT real.
    // busy_timeout: under WAL a writer can still hit a transient lock (another
    // writer / a checkpoint); wait up to 5s for it instead of erroring the
    // caller with SQLITE_BUSY immediately. Robustness, not a perf change.
    // Run as `execute_batch` because PRAGMA journal_mode returns rows.
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;\
         PRAGMA synchronous  = NORMAL;\
         PRAGMA busy_timeout = 5000;\
         PRAGMA foreign_keys = ON;\
         PRAGMA auto_vacuum  = INCREMENTAL;",
    )
    .map_err(|e| Error::SchemaMigration(format!("set pragmas: {e}")))?;
    Ok(())
}

fn apply<F>(
    conn: &mut Connection,
    label: &str,
    expected_app_id: i32,
    latest: u32,
    sql_for: F,
) -> Result<()>
where
    F: Fn(u32) -> Option<&'static str>,
{
    let current_app_id: i32 = conn
        .query_row("PRAGMA application_id;", [], |row| row.get(0))
        .map_err(|e| Error::SchemaMigration(format!("read application_id: {e}")))?;
    let current_version: u32 = conn
        .query_row("PRAGMA user_version;", [], |row| row.get(0))
        .map_err(|e| Error::SchemaMigration(format!("read user_version: {e}")))?;

    // Fresh DB carries application_id=0 and user_version=0; both will be
    // set by the v1 migration. A non-zero application_id that doesn't
    // match means we've opened the wrong file.
    if current_app_id != 0 && current_app_id != expected_app_id {
        return Err(Error::SchemaMigration(format!(
            "{label}: wrong application_id 0x{current_app_id:08x} (expected 0x{expected_app_id:08x})"
        )));
    }

    if current_version > latest {
        return Err(Error::SchemaMigration(format!(
            "{label}: db at v{current_version}, code only knows up to v{latest}"
        )));
    }

    for v in (current_version + 1)..=latest {
        let sql = sql_for(v).ok_or_else(|| {
            Error::SchemaMigration(format!("{label}: missing migration for v{v}"))
        })?;
        let tx = conn
            .transaction()
            .map_err(|e| Error::SchemaMigration(format!("{label}: begin v{v}: {e}")))?;
        tx.execute_batch(sql)
            .map_err(|e| Error::SchemaMigration(format!("{label}: apply v{v}: {e}")))?;
        // The vN.sql sets PRAGMA user_version inline, but PRAGMA inside
        // a transaction in some SQLite versions is silently ignored —
        // re-set after commit to be sure.
        tx.commit()
            .map_err(|e| Error::SchemaMigration(format!("{label}: commit v{v}: {e}")))?;
        conn.pragma_update(None, "user_version", v)
            .map_err(|e| Error::SchemaMigration(format!("{label}: bump user_version: {e}")))?;
        conn.pragma_update(None, "application_id", expected_app_id)
            .map_err(|e| Error::SchemaMigration(format!("{label}: stamp application_id: {e}")))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_fresh_apply_latest() {
        let mut conn = Connection::open_in_memory().unwrap();
        apply_data_migrations(&mut conn).unwrap();
        let v: u32 = conn
            .query_row("PRAGMA user_version;", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, DATA_LATEST);
        let app: i32 = conn
            .query_row("PRAGMA application_id;", [], |r| r.get(0))
            .unwrap();
        assert_eq!(app, DATA_APPLICATION_ID);
        // v1 tables exist.
        for tbl in [
            "container",
            "item",
            "membership",
            "container_change",
            "schema_meta",
        ] {
            let n: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1;",
                    [tbl],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "missing v1 table {tbl}");
        }
        // v1.1 / v1.2 tables (incl. virtual mail_search) exist.
        for tbl in [
            "set_state",
            "set_change",
            "blob_refs",
            "mail_retrain_outbox",
            "mail_threads",
            "mail_search",
            "mail_envelopes",
        ] {
            let n: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master \
                     WHERE name=?1 AND type IN ('table','virtual table');",
                    [tbl],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(n >= 1, "missing v1.1 object {tbl}");
        }
        // v1.4 table (mailbox-changes-substrate Phase 1) exists.
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type='table' AND name='container_change_set';",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "missing v1.4 table container_change_set");
        // v1.5 columns (mailbox-changes-substrate Phase 3) on set_state.
        // PRAGMA table_info returns (cid, name, type, notnull, dflt_value, pk).
        let cols: Vec<(String, i64, Option<String>)> = conn
            .prepare("PRAGMA table_info(set_state);")
            .unwrap()
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, Option<String>>(4)?,
                ))
            })
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        for col in ["container_change_set_floor", "set_change_floor"] {
            let row = cols
                .iter()
                .find(|(n, _, _)| n == col)
                .unwrap_or_else(|| panic!("missing v1.5 column {col}"));
            assert_eq!(row.1, 1, "v1.5 column {col} should be NOT NULL");
            assert_eq!(
                row.2.as_deref(),
                Some("0"),
                "v1.5 column {col} should default to 0"
            );
        }
        // Indexes the allocator + sweepers depend on.
        for idx in [
            "set_change_container_idx",
            "blob_refs_hash_idx",
            "blob_refs_expires_idx",
            "mail_retrain_outbox_attempts_idx",
            "mail_threads_thread_idx",
            "container_change_set_container_idx",
        ] {
            let n: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name=?1;",
                    [idx],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "missing v1.1 index {idx}");
        }
    }

    /// Simulate an existing v1 database (built by a prior release that
    /// only knew about `v1.sql`), write representative rows in every
    /// v1 table, then run the current migration runner and verify
    /// (a) later-version tables landed, (b) `user_version` advanced to
    /// `DATA_LATEST`, (c) the pre-existing v1 rows are still intact
    /// byte-for-byte across the full v1 → latest arc.
    #[test]
    fn data_v1_to_latest_preserves_existing_data() {
        let mut conn = Connection::open_in_memory().unwrap();
        set_pragmas(&conn).unwrap();
        // Apply only v1 by hand — bypass the runner so we land at
        // user_version=1 the same way an old binary would have.
        conn.execute_batch(DATA_V1_SQL).unwrap();
        conn.pragma_update(None, "user_version", 1u32).unwrap();
        conn.pragma_update(None, "application_id", DATA_APPLICATION_ID)
            .unwrap();

        // Seed one row per v1 table so an accidental DROP/recreate in
        // v1_1.sql would surface as missing data after the upgrade.
        let now: i64 = 1_700_000_000;
        conn.execute(
            "INSERT INTO container \
             (id, parent_id, name, attrs, created_at) \
             VALUES ('c1', NULL, 'INBOX', '{}', ?1);",
            [now],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO item (id, blob_hash, size_bytes, received_at) \
             VALUES ('i1', 'deadbeef', 42, ?1);",
            [now],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO membership \
             (item_id, container_id, seq, change_seq, flags, tags, added_at) \
             VALUES ('i1', 'c1', 1, 1, 0, NULL, ?1);",
            [now],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO container_change \
             (container_id, change_seq, kind, seq, item_id, changed_at) \
             VALUES ('c1', 1, 0, 1, 'i1', ?1);",
            [now],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO schema_meta (key, value) VALUES ('marker', 'preserved');",
            [],
        )
        .unwrap();

        // Run the live migration. v1.1 step should land additively.
        apply_data_migrations(&mut conn).unwrap();

        let v: u32 = conn
            .query_row("PRAGMA user_version;", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, DATA_LATEST);

        // v1 rows survived.
        let n_container: i64 = conn
            .query_row("SELECT COUNT(*) FROM container WHERE id='c1';", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(n_container, 1);
        let marker: String = conn
            .query_row(
                "SELECT value FROM schema_meta WHERE key='marker';",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(marker, "preserved");

        // v1.1 / v1.2 / v1.4 tables are present and empty (no backfill).
        for tbl in [
            "set_state",
            "set_change",
            "blob_refs",
            "mail_threads",
            "mail_retrain_outbox",
            "mail_envelopes",
            "container_change_set",
        ] {
            let n: i64 = conn
                .query_row(&format!("SELECT COUNT(*) FROM {tbl};"), [], |r| r.get(0))
                .unwrap();
            assert_eq!(n, 0, "{tbl} should be empty after migration (no backfill)");
        }
        // v1.5 columns are present on set_state. The v1→v1.5 path
        // is the production-realistic upgrade arc, so the floor
        // columns must land via ALTER TABLE without a backfill scan.
        let cols: Vec<String> = conn
            .prepare("PRAGMA table_info(set_state);")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        for col in ["container_change_set_floor", "set_change_floor"] {
            assert!(
                cols.iter().any(|c| c == col),
                "missing v1.5 column {col} after v1→latest migration"
            );
        }
    }

    /// FTS5 smoke — verifies the bundled SQLite has FTS5 enabled and
    /// the v1.1 schema's mail_search virtual table is queryable.
    /// If this test fails on a build, the production write paths in
    /// 8e will also fail; catching it at the migration layer is the
    /// cheapest check.
    #[test]
    fn mail_search_fts5_smoke() {
        let mut conn = Connection::open_in_memory().unwrap();
        apply_data_migrations(&mut conn).unwrap();
        conn.execute(
            "INSERT INTO mail_search \
             (rowid, item_id, account_id, headers, subject, body_text, normalized_addrs) \
             VALUES (1, 'i1', 1, 'From: a@b\nTo: c@d', 'hello world', \
                     'lorem ipsum hello world', 'a@b,c@d');",
            [],
        )
        .unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM mail_search WHERE mail_search MATCH 'hello';",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "FTS5 MATCH should find the inserted row");
        let n_miss: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM mail_search WHERE mail_search MATCH 'nonsense';",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n_miss, 0);
    }

    /// v1.6 migration must DROP and re-create mail_search so existing
    /// v2+ databases gain the `contentless_unindexed=1` and
    /// `contentless_delete=1` options. The migrator only runs versions
    /// above the current `user_version`, so an in-place edit of
    /// `v1_1.sql` would leave existing v2+ DBs at the old definition
    /// — which is the BLOCKER this migration closes.
    #[test]
    fn data_v2_to_latest_upgrades_mail_search_options() {
        let mut conn = Connection::open_in_memory().unwrap();
        set_pragmas(&conn).unwrap();
        // Land at user_version=2 the way an old v1.1 binary would have.
        conn.execute_batch(DATA_V1_SQL).unwrap();
        conn.pragma_update(None, "user_version", 1u32).unwrap();
        conn.pragma_update(None, "application_id", DATA_APPLICATION_ID)
            .unwrap();
        conn.execute_batch(DATA_V1_1_SQL).unwrap();
        conn.pragma_update(None, "user_version", 2u32).unwrap();

        // Pre-upgrade: the v1.1 definition has neither
        // contentless_unindexed nor contentless_delete, so an INSERT
        // succeeds but its item_id reads back as NULL — making the
        // gate-2 reap `DELETE FROM mail_search WHERE item_id = ?`
        // a silent no-op. Pin that defect to prove the migration is
        // the only thing closing it.
        conn.execute(
            "INSERT INTO mail_search \
             (rowid, item_id, account_id, headers, subject, body_text, normalized_addrs) \
             VALUES (1, 'i-pre', 1, '', 'pre-upgrade', '', '');",
            [],
        )
        .unwrap();
        let pre_item_id: Option<String> = conn
            .query_row(
                "SELECT item_id FROM mail_search WHERE rowid = 1;",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            pre_item_id.is_none(),
            "v1.1 contentless mail_search returns NULL for UNINDEXED item_id (no contentless_unindexed)"
        );

        // Run the live migration. v1.6 should DROP + recreate mail_search.
        apply_data_migrations(&mut conn).unwrap();

        let v: u32 = conn
            .query_row("PRAGMA user_version;", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, DATA_LATEST);

        // Post-upgrade: DELETE works.
        conn.execute(
            "INSERT INTO mail_search \
             (rowid, item_id, account_id, headers, subject, body_text, normalized_addrs) \
             VALUES (2, 'i-post', 1, '', 'post-upgrade hello', '', '');",
            [],
        )
        .unwrap();
        let item_id_back: String = conn
            .query_row(
                "SELECT item_id FROM mail_search WHERE rowid = 2;",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            item_id_back, "i-post",
            "contentless_unindexed=1 must let UNINDEXED columns read back as bytes"
        );
        let n = conn
            .execute("DELETE FROM mail_search WHERE item_id = 'i-post';", [])
            .unwrap();
        assert_eq!(n, 1, "contentless_delete=1 must accept item_id predicate");
        let remaining: i64 = conn
            .query_row("SELECT COUNT(*) FROM mail_search;", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, 0, "DELETE must actually remove the row");
    }

    /// `seed_set_state` is idempotent and exposes the singleton row
    /// the §2 allocator pattern depends on. The defensive
    /// `INSERT OR IGNORE` in the allocator self-heals if a future
    /// caller forgets to seed; this test verifies the normal path.
    #[test]
    fn seed_set_state_is_idempotent() {
        let mut conn = Connection::open_in_memory().unwrap();
        apply_data_migrations(&mut conn).unwrap();
        let set = SetId(uuid::Uuid::nil());
        seed_set_state(&conn, &set).unwrap();
        seed_set_state(&conn, &set).unwrap();
        let (id, seq): (String, i64) = conn
            .query_row("SELECT set_id, set_change_seq FROM set_state;", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(id, set.0.to_string());
        assert_eq!(seq, 0);
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM set_state;", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }

    /// `container_change_set.payload` is NOT NULL by design — every
    /// documented kind has a payload shape and a NULL would crash the
    /// read-side decoder. Pin the schema constraint so a future
    /// loosening of v1_4.sql surfaces here, not in production.
    #[test]
    fn container_change_set_rejects_null_payload() {
        let mut conn = Connection::open_in_memory().unwrap();
        apply_data_migrations(&mut conn).unwrap();
        let err = conn
            .execute(
                "INSERT INTO container_change_set \
                 (container_id, kind, payload, changed_at) \
                 VALUES ('c1', 'CONTAINER_CREATED', NULL, 1700000000);",
                [],
            )
            .unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("NOT NULL") || msg.contains("constraint"),
            "expected NOT NULL constraint failure, got: {msg}"
        );
    }

    /// Pin AUTOINCREMENT semantics: after delete-then-reinsert, the
    /// new row must get a strictly-greater seq, not reuse the deleted
    /// rowid. This is the property paginating JMAP Mailbox/changes
    /// clients depend on; if a future migration accidentally drops
    /// AUTOINCREMENT (plain INTEGER PRIMARY KEY), this test catches it.
    #[test]
    fn container_change_set_seq_is_strictly_monotonic_across_delete() {
        let mut conn = Connection::open_in_memory().unwrap();
        apply_data_migrations(&mut conn).unwrap();
        conn.execute(
            "INSERT INTO container_change_set \
             (container_id, kind, payload, changed_at) \
             VALUES ('c1', 'CONTAINER_CREATED', '{}', 1);",
            [],
        )
        .unwrap();
        let first_seq: i64 = conn
            .query_row(
                "SELECT container_change_set_seq FROM container_change_set;",
                [],
                |r| r.get(0),
            )
            .unwrap();
        conn.execute("DELETE FROM container_change_set;", [])
            .unwrap();
        conn.execute(
            "INSERT INTO container_change_set \
             (container_id, kind, payload, changed_at) \
             VALUES ('c2', 'CONTAINER_CREATED', '{}', 2);",
            [],
        )
        .unwrap();
        let second_seq: i64 = conn
            .query_row(
                "SELECT container_change_set_seq FROM container_change_set;",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            second_seq > first_seq,
            "expected strict monotonicity (AUTOINCREMENT), got first={first_seq} second={second_seq}"
        );
    }

    /// v1.7 must land `mail_envelopes.normalized_subject TEXT NOT
    /// NULL DEFAULT ''` plus `mail_envelopes_normsubj_idx`. The
    /// maild threading-fallback query depends on the index for
    /// per-account lookup performance; without it, every fallback
    /// resolution would full-scan `mail_envelopes`. Pre-migration
    /// rows MUST carry the empty default so the maild-side
    /// `if !normalized.is_empty()` skip stays load-bearing.
    #[test]
    fn data_v1_7_lands_normalized_subject_column_and_index() {
        let mut conn = Connection::open_in_memory().unwrap();
        apply_data_migrations(&mut conn).unwrap();
        // Column shape: name, not_null, dflt_value, type.
        let cols: Vec<(String, i64, Option<String>, String)> = conn
            .prepare("PRAGMA table_info(mail_envelopes);")
            .unwrap()
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, String>(2)?,
                ))
            })
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        let col = cols
            .iter()
            .find(|(n, _, _, _)| n == "normalized_subject")
            .expect("v1.7 must add mail_envelopes.normalized_subject");
        assert_eq!(col.1, 1, "normalized_subject must be NOT NULL");
        assert_eq!(
            col.2.as_deref(),
            Some("''"),
            "normalized_subject default must be empty string"
        );
        assert_eq!(col.3, "TEXT", "normalized_subject must be TEXT");

        // Composite index landed and covers BOTH the WHERE filter
        // and the ORDER BY chain used by the cosmix-maild subject-
        // fallback lookup. Asserting the exact column order pins
        // the index-covering property; if a future maintainer drops
        // a column or reorders the keys the threading-fallback query
        // would silently regress to a TEMP B-TREE sort.
        let n_idx: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type='index' AND name='mail_envelopes_normsubj_idx';",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n_idx, 1, "missing v1.7 index mail_envelopes_normsubj_idx");
        let idx_cols: Vec<String> = conn
            .prepare(
                "SELECT name FROM pragma_index_info('mail_envelopes_normsubj_idx') \
                 ORDER BY seqno;",
            )
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert_eq!(
            idx_cols,
            vec![
                "normalized_subject".to_string(),
                "date_ts".to_string(),
                "message_id".to_string(),
                "item_id".to_string(),
            ],
            "mail_envelopes_normsubj_idx must be a composite covering the threading-fallback ORDER BY"
        );

        // Default backfill behaviour: an INSERT that omits
        // normalized_subject must read back as empty string, not NULL
        // — the maild fallback's `is_empty()` skip relies on this.
        conn.execute(
            "INSERT INTO item (id, blob_hash, size_bytes, received_at) \
             VALUES ('i-norm', 'deadbeef', 1, 1700000000);",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO mail_envelopes \
             (item_id, from_addr, to_addrs, cc_addrs, bcc_addrs, \
              reply_to_addrs, subject, date_ts, message_id) \
             VALUES ('i-norm', 'a@b', '', '', '', '', '', 0, 'mid-1');",
            [],
        )
        .unwrap();
        let backfilled: String = conn
            .query_row(
                "SELECT normalized_subject FROM mail_envelopes WHERE item_id='i-norm';",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            backfilled, "",
            "pre-existing rows must get empty default, not NULL"
        );
    }

    /// v1.8 must give `mail_retrain_outbox.item_id` and
    /// `mail_threads.item_id` an `item(id) ON DELETE CASCADE` FK and
    /// create the `mail_search_item_ad` AFTER DELETE trigger, while
    /// preserving the exact v1.1 column shape + existing index of each
    /// rebuilt table (mail_retrain_outbox additionally gains an
    /// item_id-leading index to back the new FK). This is the
    /// structural contract that makes the item-delete reap in
    /// container.rs fire.
    #[test]
    fn data_v1_8_adds_item_fk_cascade_and_trigger() {
        let mut conn = Connection::open_in_memory().unwrap();
        apply_data_migrations(&mut conn).unwrap();

        // FK on item(id) ON DELETE CASCADE for both rebuilt tables.
        // PRAGMA foreign_key_list returns (id, seq, table, from, to,
        // on_update, on_delete, match).
        for tbl in ["mail_retrain_outbox", "mail_threads"] {
            let fks: Vec<(String, String, String)> = conn
                .prepare(&format!("PRAGMA foreign_key_list({tbl});"))
                .unwrap()
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(2)?, // referenced table
                        r.get::<_, String>(3)?, // from column
                        r.get::<_, String>(6)?, // on_delete
                    ))
                })
                .unwrap()
                .collect::<std::result::Result<_, _>>()
                .unwrap();
            let fk = fks
                .iter()
                .find(|(t, f, _)| t == "item" && f == "item_id")
                .unwrap_or_else(|| panic!("{tbl}.item_id must FK item(id)"));
            assert_eq!(
                fk.2, "CASCADE",
                "{tbl}.item_id FK must be ON DELETE CASCADE"
            );
        }

        // AFTER DELETE trigger on item present (FTS5 reap).
        let n_trig: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type='trigger' AND name='mail_search_item_ad';",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n_trig, 1, "missing v1.8 trigger mail_search_item_ad");

        // Rebuilt tables keep their existing index, and
        // mail_retrain_outbox gains an item_id-leading index to back
        // the new FK's parent-delete child probe.
        for idx in [
            "mail_retrain_outbox_attempts_idx",
            "mail_retrain_outbox_item_idx",
            "mail_threads_thread_idx",
        ] {
            let n: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name=?1;",
                    [idx],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "missing index {idx} after v1.8 rebuild");
        }

        // mail_retrain_outbox column shape preserved (name, notnull, pk).
        let cols: Vec<(String, i64, i64)> = conn
            .prepare("PRAGMA table_info(mail_retrain_outbox);")
            .unwrap()
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, i64>(5)?,
                ))
            })
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        let names: Vec<&str> = cols.iter().map(|(n, _, _)| n.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "stamp_id",
                "account_id",
                "item_id",
                "label",
                "attempts",
                "last_error",
                "created_at"
            ],
            "mail_retrain_outbox columns must match v1.1 shape"
        );
        // PRIMARY KEY (stamp_id, label) preserved.
        let pk: Vec<&str> = cols
            .iter()
            .filter(|(_, _, pk)| *pk > 0)
            .map(|(n, _, _)| n.as_str())
            .collect();
        assert_eq!(
            pk,
            vec!["stamp_id", "label"],
            "PK must be (stamp_id, label)"
        );

        // Regression guard: foreign_keys enforcement is ON after migration.
        let fk_on: i64 = conn
            .query_row("PRAGMA foreign_keys;", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            fk_on, 1,
            "foreign_keys must be ON after apply_data_migrations"
        );
    }

    /// v8→v9 migration over a DB holding dangling rows in all three
    /// sidecars must drop them: mail_retrain_outbox / mail_threads via
    /// the backfill WHERE filter, mail_search via the one-shot DELETE.
    /// Rows whose item still exists survive, and rowid order in
    /// mail_retrain_outbox is preserved (the worker drains by rowid).
    #[test]
    fn data_v8_to_9_filters_dangling_and_preserves_rowid() {
        let mut conn = Connection::open_in_memory().unwrap();
        set_pragmas(&conn).unwrap();
        // Land at user_version=8 (v1.7) the way the current release would.
        for (sql, ver) in [
            (DATA_V1_SQL, 1u32),
            (DATA_V1_1_SQL, 2),
            (DATA_V1_2_SQL, 3),
            (DATA_V1_3_SQL, 4),
            (DATA_V1_4_SQL, 5),
            (DATA_V1_5_SQL, 6),
            (DATA_V1_6_SQL, 7),
            (DATA_V1_7_SQL, 8),
        ] {
            conn.execute_batch(sql).unwrap();
            conn.pragma_update(None, "user_version", ver).unwrap();
        }
        conn.pragma_update(None, "application_id", DATA_APPLICATION_ID)
            .unwrap();

        // One live item; the rest of the sidecar rows are dangling.
        let now: i64 = 1_700_000_000;
        conn.execute(
            "INSERT INTO item (id, blob_hash, size_bytes, received_at) \
             VALUES ('live', 'deadbeef', 1, ?1);",
            [now],
        )
        .unwrap();

        // mail_retrain_outbox: rowids 1 (live), 2 (dangling), 3 (live).
        for (rowid, stamp, item, label) in [
            (1i64, "s1", "live", "junk"),
            (2, "s2", "ghost", "junk"),
            (3, "s3", "live", "ham"),
        ] {
            conn.execute(
                "INSERT INTO mail_retrain_outbox \
                 (rowid, stamp_id, account_id, item_id, label, created_at) \
                 VALUES (?1, ?2, 1, ?3, ?4, ?5);",
                rusqlite::params![rowid, stamp, item, label, now],
            )
            .unwrap();
        }
        // mail_threads: one live, one dangling.
        conn.execute(
            "INSERT INTO mail_threads (item_id, account_id, thread_id) \
             VALUES ('live', 1, 't1'), ('ghost', 1, 't2');",
            [],
        )
        .unwrap();
        // mail_search: one live, one dangling.
        conn.execute(
            "INSERT INTO mail_search \
             (item_id, account_id, headers, subject, body_text, normalized_addrs) \
             VALUES ('live', 1, '', '', '', ''), ('ghost', 1, '', '', '', '');",
            [],
        )
        .unwrap();

        apply_data_migrations(&mut conn).unwrap();
        let v: u32 = conn
            .query_row("PRAGMA user_version;", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, DATA_LATEST);

        // Dangling rows gone, live rows kept.
        let outbox: Vec<(i64, String)> = conn
            .prepare("SELECT rowid, item_id FROM mail_retrain_outbox ORDER BY rowid;")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert_eq!(
            outbox,
            vec![(1i64, "live".to_string()), (3i64, "live".to_string())],
            "dangling outbox row dropped; live rows keep original rowids in order"
        );
        let n_threads: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM mail_threads WHERE item_id='ghost';",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n_threads, 0, "dangling mail_threads row must be filtered");
        let n_search: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM mail_search WHERE item_id='ghost';",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n_search, 0, "dangling mail_search row must be reaped");
        // Live rows survived.
        let n_live_threads: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM mail_threads WHERE item_id='live';",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n_live_threads, 1);
        let n_live_search: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM mail_search WHERE item_id='live';",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n_live_search, 1);
    }

    /// The whole point of v1.8: deleting an `item` row cascades/triggers
    /// the three item-keyed sidecars to zero. This mirrors the
    /// `DELETE FROM item` the last-membership path in container.rs runs.
    #[test]
    fn data_v1_8_item_delete_reaps_all_sidecars() {
        let mut conn = Connection::open_in_memory().unwrap();
        apply_data_migrations(&mut conn).unwrap();
        let now: i64 = 1_700_000_000;
        conn.execute(
            "INSERT INTO item (id, blob_hash, size_bytes, received_at) \
             VALUES ('i1', 'deadbeef', 1, ?1);",
            [now],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO mail_retrain_outbox \
             (stamp_id, account_id, item_id, label, created_at) \
             VALUES ('s1', 1, 'i1', 'junk', ?1);",
            [now],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO mail_threads (item_id, account_id, thread_id) \
             VALUES ('i1', 1, 't1');",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO mail_search \
             (item_id, account_id, headers, subject, body_text, normalized_addrs) \
             VALUES ('i1', 1, '', '', '', '');",
            [],
        )
        .unwrap();

        conn.execute("DELETE FROM item WHERE id = 'i1';", [])
            .unwrap();

        for tbl in ["mail_retrain_outbox", "mail_threads", "mail_search"] {
            let n: i64 = conn
                .query_row(
                    &format!("SELECT COUNT(*) FROM {tbl} WHERE item_id = 'i1';"),
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 0, "{tbl} must be reaped when item is deleted");
        }
    }

    #[test]
    fn data_idempotent() {
        let mut conn = Connection::open_in_memory().unwrap();
        apply_data_migrations(&mut conn).unwrap();
        apply_data_migrations(&mut conn).unwrap();
    }

    #[test]
    fn blobs_fresh_apply_v1() {
        let mut conn = Connection::open_in_memory().unwrap();
        apply_blobs_migrations(&mut conn).unwrap();
        let v: u32 = conn
            .query_row("PRAGMA user_version;", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, BLOBS_LATEST);
        let app: i32 = conn
            .query_row("PRAGMA application_id;", [], |r| r.get(0))
            .unwrap();
        assert_eq!(app, BLOBS_APPLICATION_ID);
    }

    #[test]
    fn rejects_wrong_application_id() {
        let mut conn = Connection::open_in_memory().unwrap();
        apply_blobs_migrations(&mut conn).unwrap();
        // Now try to open it as a data DB; should fail because magic
        // mismatches.
        let err = apply_data_migrations(&mut conn).unwrap_err();
        match err {
            Error::SchemaMigration(msg) => assert!(msg.contains("wrong application_id"), "{msg}"),
            other => panic!("expected SchemaMigration error, got {other:?}"),
        }
    }
}
