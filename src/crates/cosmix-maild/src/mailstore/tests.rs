//! Phase 8d.4 — MailStore + expiry-worker integration tests.
//!
//! Acceptance bar (per `_doc/2026-04-29-cosmix-mds-plan.md` §8d):
//!
//!   * A `move_message` across the `\Junk` boundary commits both the
//!     MDS move and the `mail_retrain_outbox` row in one tx — both
//!     visible after success, neither visible if the tx rolls back.
//!     (Atomicity is by construction via `with_set_tx`; we assert
//!     observable shape on the success path and the no-junk-container
//!     path so the wiring is exercised end-to-end.)
//!
//!   * `create_blob_ref` provisions `__upload_staging__` idempotently,
//!     records a `blob_refs` row, and `resolve_blob_ref` round-trips
//!     the original `BlobHash`. A bogus or post-GC alias surfaces an
//!     error rather than a silent miss.
//!
//!   * The expiry worker reaps refs whose `expires_at` is past, leaves
//!     refs in the future or `NULL`-expiry refs alone, and tears down
//!     the temp item under the same transaction it deletes the alias.

use std::sync::Arc;

use cosmix_mds::{
    BlobHash, ChangelogStream, ContainerAttrs, ContainerId, Flags, ItemId, Mds, Membership, SetId,
    SqliteCasMds, Tags,
};
use rusqlite::params;
use tempfile::TempDir;
use uuid::Uuid;

use super::expiry::sweep_blocking;
use super::{
    AccountId, BlobId, DestroyPolicy, EmailChangeSet, EmailEnvelope, EmailHandle, EmailQueryFilter,
    EmailRecord, EmailSort, ListOpts, MailStore, MailboxRecord, MailboxRights, MailboxRole,
    RetentionParams, RetentionSweepReport, SortKey, SqliteMailStore, ThreadId, account_id_to_setid,
};
use crate::jmap::AppState;
use cosmix_mds::container::UPLOAD_STAGING_NAME;

const TEST_ACCOUNT: AccountId = 1;

fn open_mailstore() -> (TempDir, Arc<SqliteCasMds>, SqliteMailStore) {
    let d = TempDir::new().unwrap();
    let mds = Arc::new(SqliteCasMds::open(d.path()).unwrap());
    let store = SqliteMailStore::new(Arc::clone(&mds));
    (d, mds, store)
}

fn provision(store: &SqliteMailStore) -> SetId {
    store.ensure_account_set(TEST_ACCOUNT).unwrap()
}

fn mk_container(
    mds: &SqliteCasMds,
    set: &SetId,
    name: &str,
    special_use: Option<&str>,
) -> ContainerId {
    let attrs = ContainerAttrs {
        special_use: special_use.map(|s| s.to_string()),
        subscribed: false,
        extra: serde_json::json!({}),
    };
    mds.create_container(set, None, name, attrs).unwrap()
}

fn put_blob(mds: &SqliteCasMds, payload: &[u8]) -> (BlobHash, u64) {
    let h = mds.put_blob(payload).unwrap();
    (h, payload.len() as u64)
}

fn add_item_in(mds: &SqliteCasMds, set: &SetId, container: ContainerId, payload: &[u8]) -> ItemId {
    let (hash, _) = put_blob(mds, payload);
    let m = Membership {
        container,
        flags: Flags(0),
        added_at: 0,
    };
    mds.add_item(set, &hash, &[m]).unwrap().item_id
}

fn count_outbox(mds: &SqliteCasMds, set: &SetId) -> i64 {
    let mut out = 0i64;
    let _: cosmix_mds::Result<()> = mds.with_set_tx(set, |tx| {
        out = tx
            .tx()
            .query_row("SELECT COUNT(*) FROM mail_retrain_outbox", [], |r| r.get(0))
            .unwrap();
        // Always Err so the probe doesn't allocate set_change rows.
        Err(cosmix_mds::Error::Other("probe".into()))
    });
    out
}

fn count_blob_refs(mds: &SqliteCasMds, set: &SetId) -> i64 {
    let mut out = 0i64;
    let _: cosmix_mds::Result<()> = mds.with_set_tx(set, |tx| {
        out = tx
            .tx()
            .query_row("SELECT COUNT(*) FROM blob_refs", [], |r| r.get(0))
            .unwrap();
        Err(cosmix_mds::Error::Other("probe".into()))
    });
    out
}

fn count_items(mds: &SqliteCasMds, set: &SetId) -> i64 {
    let mut out = 0i64;
    let _: cosmix_mds::Result<()> = mds.with_set_tx(set, |tx| {
        out = tx
            .tx()
            .query_row("SELECT COUNT(*) FROM item", [], |r| r.get(0))
            .unwrap();
        Err(cosmix_mds::Error::Other("probe".into()))
    });
    out
}

fn count_sidecar_for_item(mds: &SqliteCasMds, set: &SetId, table: &str, item: &ItemId) -> i64 {
    let sql = format!("SELECT COUNT(*) FROM {table} WHERE item_id = ?1");
    let mut out = 0i64;
    let _: cosmix_mds::Result<()> = mds.with_set_tx(set, |tx| {
        out = tx
            .tx()
            .query_row(&sql, params![item.0.to_string()], |r| r.get(0))
            .unwrap();
        Err(cosmix_mds::Error::Other("probe".into()))
    });
    out
}

/// Insert a `mail_retrain_outbox` row directly (FK now requires the
/// `item` to exist — v1.8). Used where driving a `\Junk` move just to
/// enqueue an outbox row would be noisier than a direct insert.
fn insert_retrain_outbox(
    mds: &SqliteCasMds,
    set: &SetId,
    item: &ItemId,
    stamp_id: &str,
    label: &str,
) {
    mds.with_set_tx(set, |tx| {
        tx.tx()
            .execute(
                "INSERT INTO mail_retrain_outbox \
                 (stamp_id, account_id, item_id, label, created_at) \
                 VALUES (?1, 1, ?2, ?3, 0)",
                params![stamp_id, item.0.to_string(), label],
            )
            .unwrap();
        Ok(())
    })
    .unwrap();
}

fn outbox_label_for(mds: &SqliteCasMds, set: &SetId, stamp_id: &str) -> Option<String> {
    let mut out: Option<String> = None;
    let _: cosmix_mds::Result<()> = mds.with_set_tx(set, |tx| {
        out = tx
            .tx()
            .query_row(
                "SELECT label FROM mail_retrain_outbox WHERE stamp_id = ?1",
                params![stamp_id],
                |r| r.get(0),
            )
            .ok();
        Err(cosmix_mds::Error::Other("probe".into()))
    });
    out
}

// -------------------- move_message --------------------

#[test]
fn move_message_to_junk_writes_outbox_with_junk_label() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));
    let junk = mk_container(&mds, &set, "Junk", Some("\\Junk"));
    let item = add_item_in(&mds, &set, inbox, b"spam");

    store
        .move_message(TEST_ACCOUNT, item, inbox, junk, Flags(0), "stamp-1")
        .unwrap();

    assert_eq!(count_outbox(&mds, &set), 1);
    assert_eq!(
        outbox_label_for(&mds, &set, "stamp-1").as_deref(),
        Some("junk")
    );
}

#[test]
fn move_message_from_junk_writes_outbox_with_ham_label() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));
    let junk = mk_container(&mds, &set, "Junk", Some("\\Junk"));
    let item = add_item_in(&mds, &set, junk, b"actually-ham");

    store
        .move_message(TEST_ACCOUNT, item, junk, inbox, Flags(0), "stamp-2")
        .unwrap();

    assert_eq!(
        outbox_label_for(&mds, &set, "stamp-2").as_deref(),
        Some("ham")
    );
}

#[test]
fn move_message_no_outbox_when_neither_endpoint_is_junk() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let _junk = mk_container(&mds, &set, "Junk", Some("\\Junk"));
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));
    let archive = mk_container(&mds, &set, "Archive", Some("\\Archive"));
    let item = add_item_in(&mds, &set, inbox, b"keepsake");

    store
        .move_message(TEST_ACCOUNT, item, inbox, archive, Flags(0), "stamp-3")
        .unwrap();

    assert_eq!(count_outbox(&mds, &set), 0);
}

#[test]
fn move_message_no_outbox_when_set_has_no_junk_container() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));
    let archive = mk_container(&mds, &set, "Archive", Some("\\Archive"));
    let item = add_item_in(&mds, &set, inbox, b"hello");

    store
        .move_message(TEST_ACCOUNT, item, inbox, archive, Flags(0), "stamp-4")
        .unwrap();

    assert_eq!(count_outbox(&mds, &set), 0);
}

#[test]
fn move_message_outbox_idempotent_on_repeat_stamp() {
    // Idempotency on (stamp_id, label) — the spec's "at-least-once
    // delivery from the worker is safe" property. A second matched
    // boundary cross with the same stamp_id+label must not duplicate
    // the row. With `INSERT OR REPLACE` the PK `(stamp_id, label)`
    // still collapses to a single row (the re-drag replaces it with
    // a fresh rowid); `record_label` is idempotent on a same-label
    // re-apply, so the worker draining it twice is harmless.
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));
    let junk = mk_container(&mds, &set, "Junk", Some("\\Junk"));
    let item_a = add_item_in(&mds, &set, inbox, b"a");
    let item_b = add_item_in(&mds, &set, inbox, b"b");

    store
        .move_message(TEST_ACCOUNT, item_a, inbox, junk, Flags(0), "shared-stamp")
        .unwrap();
    store
        .move_message(TEST_ACCOUNT, item_b, inbox, junk, Flags(0), "shared-stamp")
        .unwrap();

    assert_eq!(
        count_outbox(&mds, &set),
        1,
        "second move with the same (stamp_id, label) must collapse via OR REPLACE"
    );
}

#[test]
fn move_message_rolls_back_outbox_when_move_fails() {
    // If the MDS move_item fails (here: dest doesn't exist), the
    // outbox row must NOT have been written. Atomicity is by
    // construction (single with_set_tx) but the assertion exercises
    // it end-to-end: we never reach the outbox INSERT, and even if a
    // future refactor flipped the order, the tx would roll back.
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));
    let _junk = mk_container(&mds, &set, "Junk", Some("\\Junk"));
    let item = add_item_in(&mds, &set, inbox, b"hello");

    let bogus_dest = ContainerId(uuid::Uuid::now_v7());
    let res = store.move_message(
        TEST_ACCOUNT,
        item,
        inbox,
        bogus_dest,
        Flags(0),
        "stamp-rollback",
    );
    assert!(res.is_err(), "move to a non-existent dest must fail");
    assert_eq!(count_outbox(&mds, &set), 0);
}

#[test]
fn copy_message_fails_when_src_membership_missing_after_snapshot() {
    // T15 regression — RFC 9051 §6.4.7 source-specific inheritance
    // must not silently fall back to empty flags when a TOCTOU race
    // drops `src`'s membership between IMAP snapshot and the copy tx.
    //
    // Setup: item lives in BOTH inbox (the source) and archive
    // (sibling). Remove the inbox membership out-of-band. Then call
    // `copy_message(.., inbox -> junk)`. The item still exists via
    // its archive membership, so the substrate-level item lookup
    // succeeds — without the source-membership precondition the
    // copy would mint a junk-bound row with `Flags(0)` and zero
    // tags, dropping the user's source flags. The fixed primitive
    // must fail the call and leave `junk` empty.
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));
    let archive = mk_container(&mds, &set, "Archive", Some("\\Archive"));
    let junk = mk_container(&mds, &set, "Junk", Some("\\Junk"));

    // Add the item with memberships in both inbox and archive.
    let (hash, _) = put_blob(&mds, b"two-place item");
    let m_inbox = Membership {
        container: inbox,
        flags: Flags(0),
        added_at: 0,
    };
    let m_archive = Membership {
        container: archive,
        flags: Flags(0),
        added_at: 0,
    };
    let item = mds
        .add_item(&set, &hash, &[m_inbox, m_archive])
        .unwrap()
        .item_id;

    // Simulate the snapshot/write race: drop inbox's membership
    // before the COPY tx runs.
    mds.with_set_tx(&set, |tx| tx.remove_membership(&item, &inbox))
        .unwrap();

    let stamp = item.0.to_string();
    let res = store.copy_message(TEST_ACCOUNT, item, inbox, junk, &stamp, false);
    assert!(
        res.is_err(),
        "copy_message must fail when src membership is missing at write time"
    );

    // Dest must remain empty — the failed copy must not have minted
    // a Flags(0) junk membership.
    let dest_listing = store
        .list_emails_in_mailbox(TEST_ACCOUNT, junk, ListOpts::default())
        .unwrap();
    assert!(
        dest_listing.is_empty(),
        "dest mailbox must have no membership after failed COPY (saw {} rows)",
        dest_listing.len()
    );

    // Retrain outbox must not have grown either.
    assert_eq!(count_outbox(&mds, &set), 0);
}

#[test]
fn move_message_returned_uids_match_list_emails_in_mailbox() {
    // P7 UIDPLUS / COPYUID regression: the (src_uid, dest_uid)
    // returned from `move_message` must mirror the per-membership
    // (seq.0, container.seq_validity) as projected by
    // `list_emails_in_mailbox` — src_uid against the pre-move dump,
    // dest_uid against the post-move dump. Catches read-skew if the
    // returned UIDs were ever split out of the same `with_set_tx`.
    let (_d, mds, store) = open_mailstore();
    let _set = provision(&store);
    let inbox = store
        .create_mailbox(TEST_ACCOUNT, "Inbox", None, default_attrs())
        .unwrap();
    let archive = store
        .create_mailbox(TEST_ACCOUNT, "Archive", None, default_attrs())
        .unwrap();

    // Seed src and dest with *asymmetric* counts so the moved item
    // lands at distinct UIDs in src vs dest — otherwise the test
    // would pass even if seq_src was wrongly populated from seq_dest
    // (or vice versa) because both happen to coincide when both
    // mailboxes have the same prior length. The container-local UID
    // grows on each insert into that container, so 2 seeds in inbox
    // and 4 seeds in archive yield src_uid=3, dest_uid=5 after the
    // move.
    let (seed_hash, _) = put_blob(&mds, b"seed for uid offset");
    for _ in 0..2 {
        let _ = store
            .create_email(
                TEST_ACCOUNT,
                inbox,
                seed_hash,
                sample_envelope(),
                &[],
                Flags(0),
                Tags::new(),
                1_699_999_999_000,
            )
            .unwrap();
    }
    for _ in 0..4 {
        let _ = store
            .create_email(
                TEST_ACCOUNT,
                archive,
                seed_hash,
                sample_envelope(),
                &[],
                Flags(0),
                Tags::new(),
                1_699_999_999_000,
            )
            .unwrap();
    }

    let (hash, _) = put_blob(&mds, b"copyuid regression");
    let env = sample_envelope();
    let (id, seed_uid) = store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            hash,
            env,
            &[],
            Flags(0),
            Tags::new(),
            1_700_000_000_000,
        )
        .unwrap();
    // After two seeds, the third inbox insert lands at UID 3.
    assert_eq!(
        seed_uid.uid, 3,
        "test invariant: third inbox insert must be UID 3"
    );

    // Pre-move: src membership exists in inbox at the seeded uid.
    let pre = store
        .list_emails_in_mailbox(
            TEST_ACCOUNT,
            inbox,
            ListOpts {
                limit: 16,
                offset: 0,
                sort_by: SortKey::Seq,
            },
        )
        .unwrap();
    let pre_row = pre
        .iter()
        .find(|h| h.id == id)
        .expect("item must be visible in src mailbox before the move");
    let expected_src_uid = pre_row.uid;

    let (src_uid, dest_uid) = store
        .move_message(TEST_ACCOUNT, id, inbox, archive, Flags(0), "p7-move-uid")
        .unwrap();

    assert_eq!(src_uid.container_id, inbox);
    assert_eq!(dest_uid.container_id, archive);
    assert_ne!(
        src_uid.uid, dest_uid.uid,
        "test invariant: src and dest UIDs must differ so a swapped seq_src/seq_dest is detectable"
    );
    assert_eq!(
        src_uid.uid, expected_src_uid,
        "move_message src_uid must equal pre-move list_emails uid"
    );

    // Post-move: src is gone, dest has the row at the returned uid.
    let post_src = store
        .list_emails_in_mailbox(
            TEST_ACCOUNT,
            inbox,
            ListOpts {
                limit: 16,
                offset: 0,
                sort_by: SortKey::Seq,
            },
        )
        .unwrap();
    assert!(
        post_src.iter().all(|h| h.id != id),
        "moved item must not remain in src mailbox listing"
    );
    let post_dest = store
        .list_emails_in_mailbox(
            TEST_ACCOUNT,
            archive,
            ListOpts {
                limit: 16,
                offset: 0,
                sort_by: SortKey::Seq,
            },
        )
        .unwrap();
    let dest_row = post_dest
        .iter()
        .find(|h| h.id == id)
        .expect("moved item must be visible in dest mailbox after the move");
    assert_eq!(
        dest_row.uid, dest_uid.uid,
        "move_message dest_uid must equal post-move list_emails uid"
    );
}

#[test]
fn copy_message_returned_uid_matches_list_emails_in_mailbox() {
    // P7 UIDPLUS / COPYUID regression — sibling of
    // `move_message_returned_uids_match_list_emails_in_mailbox` for
    // the COPY path. The returned dest `MembershipUid` must mirror
    // the per-membership (seq.0, container.seq_validity) projected
    // by `list_emails_in_mailbox(dest)`, AND the source membership
    // must survive at its original uid (COPY preserves src per RFC
    // 9051 §6.4.7, unlike MOVE). The single returned MembershipUid
    // is dest-only by design: IMAP UID COPY resolves the src-set
    // from its SELECTED-state snapshot before invoking this
    // primitive (see `copy_message` doc-comment on `MailStore`).
    let (_d, mds, store) = open_mailstore();
    let _set = provision(&store);
    let inbox = store
        .create_mailbox(TEST_ACCOUNT, "Inbox", None, default_attrs())
        .unwrap();
    let archive = store
        .create_mailbox(TEST_ACCOUNT, "Archive", None, default_attrs())
        .unwrap();

    // Asymmetric seeds so the copied row lands at distinct UIDs in
    // src vs dest — matches the move-sibling's seq_src/seq_dest
    // detection trick. 2 inbox seeds + 4 archive seeds → src_uid=3,
    // dest_uid=5 after the copy.
    let (seed_hash, _) = put_blob(&mds, b"seed for copyuid offset");
    for _ in 0..2 {
        let _ = store
            .create_email(
                TEST_ACCOUNT,
                inbox,
                seed_hash,
                sample_envelope(),
                &[],
                Flags(0),
                Tags::new(),
                1_699_999_999_000,
            )
            .unwrap();
    }
    for _ in 0..4 {
        let _ = store
            .create_email(
                TEST_ACCOUNT,
                archive,
                seed_hash,
                sample_envelope(),
                &[],
                Flags(0),
                Tags::new(),
                1_699_999_999_000,
            )
            .unwrap();
    }

    let (hash, _) = put_blob(&mds, b"copyuid returned-uid regression");
    let env = sample_envelope();
    let (id, src_uid_at_create) = store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            hash,
            env,
            &[],
            Flags(0),
            Tags::new(),
            1_700_000_000_000,
        )
        .unwrap();
    assert_eq!(
        src_uid_at_create.uid, 3,
        "test invariant: third inbox insert must be UID 3"
    );

    // Pre-copy: src membership exists in inbox at the seeded uid.
    let pre_src = store
        .list_emails_in_mailbox(
            TEST_ACCOUNT,
            inbox,
            ListOpts {
                limit: 16,
                offset: 0,
                sort_by: SortKey::Seq,
            },
        )
        .unwrap();
    let pre_src_row = pre_src
        .iter()
        .find(|h| h.id == id)
        .expect("item must be visible in src mailbox before the copy");
    let expected_src_uid = pre_src_row.uid;

    let stamp = id.0.to_string();
    let dest_uid = store
        .copy_message(TEST_ACCOUNT, id, inbox, archive, &stamp, false)
        .unwrap();

    assert_eq!(
        dest_uid.container_id, archive,
        "returned MembershipUid.container_id must be the dest mailbox"
    );
    assert!(dest_uid.uid > 0, "COPYUID dest uid must be > 0");
    assert_ne!(
        dest_uid.uid, expected_src_uid,
        "test invariant: src and dest UIDs must differ so a swapped seq could be detected"
    );

    // Post-copy: src membership is preserved at its original uid
    // (COPY, not MOVE — RFC 9051 §6.4.7).
    let post_src = store
        .list_emails_in_mailbox(
            TEST_ACCOUNT,
            inbox,
            ListOpts {
                limit: 16,
                offset: 0,
                sort_by: SortKey::Seq,
            },
        )
        .unwrap();
    let post_src_row = post_src
        .iter()
        .find(|h| h.id == id)
        .expect("item must remain in src mailbox after COPY");
    assert_eq!(
        post_src_row.uid, expected_src_uid,
        "src uid must not change across COPY (UID stability — RFC 9051 §2.3.1.1)"
    );

    // Dest: row visible at the returned uid.
    let post_dest = store
        .list_emails_in_mailbox(
            TEST_ACCOUNT,
            archive,
            ListOpts {
                limit: 16,
                offset: 0,
                sort_by: SortKey::Seq,
            },
        )
        .unwrap();
    let dest_row = post_dest
        .iter()
        .find(|h| h.id == id)
        .expect("copied item must be visible in dest mailbox after the copy");
    assert_eq!(
        dest_row.uid, dest_uid.uid,
        "copy_message returned uid must equal list_emails_in_mailbox(dest) uid"
    );

    // uid_validity sanity: the returned uid_validity must match
    // what list_mailboxes(dest) projects, so an IMAP COPYUID
    // response built from this MembershipUid would carry a
    // validity consistent with what the client sees on SELECT.
    let dest_mailbox = store
        .list_mailboxes(TEST_ACCOUNT)
        .unwrap()
        .into_iter()
        .find(|m| m.id == archive)
        .expect("dest mailbox must be visible in list_mailboxes");
    assert_eq!(
        dest_uid.uid_validity, dest_mailbox.uid_validity,
        "copy_message returned uid_validity must match list_mailboxes(dest).uid_validity"
    );
}

// -------------------- blob_refs --------------------

#[test]
fn create_blob_ref_then_resolve_round_trips() {
    let (_d, mds, store) = open_mailstore();
    let _set = provision(&store);
    let bytes = b"upload-bytes";
    let (hash, size) = put_blob(&mds, bytes);

    let blob_id = store
        .create_blob_ref(TEST_ACCOUNT, hash, size, 9_999_999_999)
        .unwrap();

    let resolved = store.resolve_blob_ref(TEST_ACCOUNT, blob_id).unwrap();
    match resolved {
        crate::mailstore::BlobRefLookup::Found(h) => assert_eq!(h, hash),
        other => panic!("expected Found, got {other:?}"),
    }
}

#[test]
fn create_blob_ref_provisions_staging_idempotently() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let (hash_a, size_a) = put_blob(&mds, b"alpha");
    let (hash_b, size_b) = put_blob(&mds, b"beta");

    let _a = store
        .create_blob_ref(TEST_ACCOUNT, hash_a, size_a, 1)
        .unwrap();
    let _b = store
        .create_blob_ref(TEST_ACCOUNT, hash_b, size_b, 1)
        .unwrap();

    // Exactly one __upload_staging__ container under the account set.
    let mut count = 0i64;
    let _: cosmix_mds::Result<()> = mds.with_set_tx(&set, |tx| {
        count = tx
            .tx()
            .query_row(
                "SELECT COUNT(*) FROM container WHERE name = ?1 AND parent_id IS NULL",
                params![cosmix_mds::container::UPLOAD_STAGING_NAME],
                |r| r.get(0),
            )
            .unwrap();
        Err(cosmix_mds::Error::Other("probe".into()))
    });
    assert_eq!(count, 1);
    assert_eq!(count_blob_refs(&mds, &set), 2);
}

#[test]
fn resolve_blob_ref_returns_not_found_for_missing_alias() {
    let (_d, _mds, store) = open_mailstore();
    let _set = provision(&store);
    let bogus = BlobId(uuid::Uuid::now_v7());
    let res = store
        .resolve_blob_ref(TEST_ACCOUNT, bogus)
        .expect("missing-alias lookup must not error");
    assert!(
        matches!(res, crate::mailstore::BlobRefLookup::NotFound),
        "missing alias must surface as NotFound, got {res:?}"
    );
}

// -------------------- expiry worker --------------------

#[test]
fn expiry_sweep_reaps_expired_ref_and_temp_item() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let (hash, size) = put_blob(&mds, b"upload");
    let _blob_id = store
        .create_blob_ref(TEST_ACCOUNT, hash, size, 100) // expires_at = 100s
        .unwrap();

    assert_eq!(count_blob_refs(&mds, &set), 1);
    assert_eq!(count_items(&mds, &set), 1);

    let reaped = sweep_blocking(&mds, 1_000).unwrap(); // now > 100
    assert_eq!(reaped, 1);
    assert_eq!(count_blob_refs(&mds, &set), 0);
    assert_eq!(
        count_items(&mds, &set),
        0,
        "temp item must be removed alongside the alias"
    );
}

#[test]
fn expiry_sweep_keeps_unexpired_refs() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let (hash, size) = put_blob(&mds, b"upload");
    let _ = store
        .create_blob_ref(TEST_ACCOUNT, hash, size, 1_000_000)
        .unwrap();

    let reaped = sweep_blocking(&mds, 1_000).unwrap(); // now < 1_000_000
    assert_eq!(reaped, 0);
    assert_eq!(count_blob_refs(&mds, &set), 1);
    assert_eq!(count_items(&mds, &set), 1);
}

/// Insert a `blob_refs` row directly with the persisted-alias shape
/// (`temp_item_id = NULL, expires_at = NULL`). The public `MailStore`
/// surface only mints the upload-staging shape today; the persisted
/// path lands with the JMAP migration (Phase A). Until then, drive the
/// fixture from the test so the worker's "leave NULL-expiry rows
/// alone" property is exercised on the real shape.
fn insert_persisted_alias(mds: &SqliteCasMds, set: &SetId, account: AccountId, hash: &BlobHash) {
    let blob_id = uuid::Uuid::now_v7().to_string();
    let blob_hex = cosmix_mds::blob::hex(hash);
    mds.with_set_tx(set, |tx| {
        tx.tx()
            .execute(
                "INSERT INTO blob_refs \
                 (blob_id, account_id, blob_hash, created_at, expires_at, temp_item_id) \
                 VALUES (?1, ?2, ?3, 0, NULL, NULL)",
                params![blob_id, account, blob_hex],
            )
            .unwrap();
        Ok(())
    })
    .unwrap();
}

#[test]
fn expiry_sweep_keeps_null_expiry_refs() {
    // Refs minted from persisted email carry expires_at = NULL (and
    // temp_item_id = NULL) and must never be touched by the worker —
    // the underlying blob is kept alive by an `item.blob_hash`
    // reference on an already-stored mail item, not by the alias.
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let (hash, _size) = put_blob(&mds, b"persisted");
    insert_persisted_alias(&mds, &set, TEST_ACCOUNT, &hash);

    let reaped = sweep_blocking(&mds, i64::MAX).unwrap();
    assert_eq!(reaped, 0);
    assert_eq!(count_blob_refs(&mds, &set), 1);
}

#[test]
fn expiry_sweep_handles_set_with_no_refs() {
    let (_d, mds, store) = open_mailstore();
    let _set = provision(&store);
    let reaped = sweep_blocking(&mds, i64::MAX).unwrap();
    assert_eq!(reaped, 0);
}

#[test]
fn expiry_sweep_only_touches_expired_when_mixed() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let (h1, s1) = put_blob(&mds, b"expired");
    let (h2, s2) = put_blob(&mds, b"fresh");
    let (h3, s3) = put_blob(&mds, b"persistent");

    let _expired = store.create_blob_ref(TEST_ACCOUNT, h1, s1, 100).unwrap();
    let _fresh = store
        .create_blob_ref(TEST_ACCOUNT, h2, s2, 1_000_000)
        .unwrap();
    insert_persisted_alias(&mds, &set, TEST_ACCOUNT, &h3);
    // Two staging items pinned by upload refs; the persisted alias
    // carries no temp item.
    let _ = s3;

    let reaped = sweep_blocking(&mds, 1_000).unwrap();
    assert_eq!(reaped, 1);
    assert_eq!(count_blob_refs(&mds, &set), 2);
    // `_expired` reaped its temp item; `_fresh` keeps its temp item;
    // the persisted alias never owned one.
    assert_eq!(count_items(&mds, &set), 1);
}

// -------------------- setid invariant --------------------

#[test]
fn ensure_account_set_is_idempotent() {
    let (_d, _mds, store) = open_mailstore();
    let s1 = store.ensure_account_set(TEST_ACCOUNT).unwrap();
    let s2 = store.ensure_account_set(TEST_ACCOUNT).unwrap();
    assert_eq!(s1, s2);
    assert_eq!(s1, account_id_to_setid(TEST_ACCOUNT));
}

// -------------------- AppState wiring (Task 0.3) --------------------

/// Phase A Task 0.3 — `AppState` carries a
/// `mailstore: Arc<SqliteMailStore>` field reachable through the
/// shared application state every JMAP / SMTP handler receives.
///
/// This is a compile-time contract test: if the field is removed,
/// renamed, or re-typed, the test fails to compile. The full-fixture
/// AppState construction (Db + DefaultClassifier with a real
/// StorageBackend) lands as part of the e2e smoke test in Phase 7;
/// here we only need the type-level guarantee.
#[allow(dead_code)]
fn _appstate_mailstore_field_contract(state: &AppState) -> &Arc<SqliteMailStore> {
    &state.mailstore
}

#[test]
fn mailstore_mds_accessor_preserves_arc_identity() {
    let (_d, mds, store) = open_mailstore();
    let mailstore = Arc::new(store);
    assert!(Arc::ptr_eq(mailstore.mds(), &mds));
}

// -------------------- list_mailboxes (Task 1.1) ----------------------

/// Empty account (no set provisioned yet) returns an empty list, not
/// an error. JMAP `Mailbox/get` on a brand-new account should hand back
/// `[]`, never panic on a missing per-account set.
#[test]
fn list_mailboxes_empty_account_is_empty_vec() {
    let (_d, _mds, store) = open_mailstore();
    let rows = store.list_mailboxes(TEST_ACCOUNT).unwrap();
    assert!(rows.is_empty(), "expected empty list, got {rows:?}");
}

/// Three real mailboxes round-trip with their counts. The
/// `__upload_staging__` container provisioned by `create_blob_ref` is
/// filtered out of the JMAP-visible listing.
#[test]
fn list_mailboxes_returns_user_containers_with_counts_and_filters_staging() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);

    // Three user-visible mailboxes; one with two messages, one with
    // one message, one empty.
    let inbox = mk_container(&mds, &set, "Inbox", None);
    let archive = mk_container(&mds, &set, "Archive", None);
    let _empty = mk_container(&mds, &set, "Drafts", None);
    let _i1 = add_item_in(&mds, &set, inbox, b"hello");
    let _i2 = add_item_in(&mds, &set, inbox, b"world");
    let _a1 = add_item_in(&mds, &set, archive, b"older");

    // Provision the upload-staging container by minting a blob_ref.
    // This is the same path JMAP `Email/upload` exercises.
    let (hash, size) = put_blob(&mds, b"upload");
    let _bid = store
        .create_blob_ref(TEST_ACCOUNT, hash, size, i64::MAX)
        .unwrap();

    let rows = store.list_mailboxes(TEST_ACCOUNT).unwrap();
    let names: std::collections::BTreeSet<&str> = rows.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(
        names,
        ["Archive", "Drafts", "Inbox"]
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>(),
        "list should return exactly the three user mailboxes (got {names:?})"
    );

    let by_name: std::collections::HashMap<&str, &MailboxRecord> =
        rows.iter().map(|r| (r.name.as_str(), r)).collect();
    assert_eq!(by_name["Inbox"].total_emails, 2);
    assert_eq!(by_name["Archive"].total_emails, 1);
    assert_eq!(by_name["Drafts"].total_emails, 0);
    // Newly-added items are unread by default (Flags(0), no \Seen bit).
    assert_eq!(by_name["Inbox"].unread_emails, 2);
    assert_eq!(by_name["Archive"].unread_emails, 1);
    assert_eq!(by_name["Drafts"].unread_emails, 0);

    // ContainerId / parent / attrs round-trip.
    assert_eq!(by_name["Inbox"].id, inbox);
    assert!(by_name["Inbox"].parent.is_none());
    assert!(by_name["Inbox"].attrs.special_use.is_none());

    // JMAP `Mailbox` projection defaults — no `special_use` ⇒
    // `role: None`; empty `extra` ⇒ `sort_order: 0`; passthrough
    // `subscribed=false` from `mk_container`; missing
    // `jmap_my_rights` ⇒ owner-of-mailbox full rights.
    assert!(by_name["Inbox"].role.is_none());
    assert_eq!(by_name["Inbox"].sort_order, 0);
    assert!(!by_name["Inbox"].is_subscribed);
    assert_eq!(by_name["Inbox"].my_rights, MailboxRights::full());
}

// -------------------- Task 2.0 (JMAP MailboxRecord fields) --------------------

#[test]
fn list_mailboxes_maps_special_use_to_jmap_role() {
    // Each IMAP `\Special-Use` value (RFC 6154) projects to its
    // matching JMAP role token; unknown values project as `None`.
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let _inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));
    let _sent = mk_container(&mds, &set, "Sent", Some("\\Sent"));
    let _junk = mk_container(&mds, &set, "Junk", Some("\\Junk"));
    let _trash = mk_container(&mds, &set, "Trash", Some("\\Trash"));
    let _drafts = mk_container(&mds, &set, "Drafts", Some("\\Drafts"));
    let _archive = mk_container(&mds, &set, "Archive", Some("\\Archive"));
    let _flagged = mk_container(&mds, &set, "Flagged", Some("\\Flagged"));
    let _all = mk_container(&mds, &set, "All", Some("\\All"));
    let _important = mk_container(&mds, &set, "Important", Some("\\Important"));
    let _other = mk_container(&mds, &set, "Other", Some("\\NoiseAttribute"));
    let _unmarked = mk_container(&mds, &set, "Unmarked", None);

    let rows = store.list_mailboxes(TEST_ACCOUNT).unwrap();
    let by_name: std::collections::HashMap<&str, &MailboxRecord> =
        rows.iter().map(|r| (r.name.as_str(), r)).collect();
    assert_eq!(by_name["Inbox"].role, Some(MailboxRole::Inbox));
    assert_eq!(by_name["Sent"].role, Some(MailboxRole::Sent));
    assert_eq!(by_name["Junk"].role, Some(MailboxRole::Junk));
    assert_eq!(by_name["Trash"].role, Some(MailboxRole::Trash));
    assert_eq!(by_name["Drafts"].role, Some(MailboxRole::Drafts));
    assert_eq!(by_name["Archive"].role, Some(MailboxRole::Archive));
    assert_eq!(by_name["Flagged"].role, Some(MailboxRole::Flagged));
    assert_eq!(by_name["All"].role, Some(MailboxRole::All));
    assert_eq!(by_name["Important"].role, Some(MailboxRole::Important));
    // Unknown attribute → JMAP role projects as None rather than
    // misclassifying the mailbox.
    assert!(by_name["Other"].role.is_none());
    assert!(by_name["Unmarked"].role.is_none());
}

#[test]
fn list_mailboxes_reads_sort_order_and_my_rights_from_extra() {
    // Write a container whose `attrs.extra` carries explicit
    // `jmap_sort_order` and `jmap_my_rights` payloads, and confirm
    // the adapter round-trips them. Crafted partial `my_rights` to
    // also exercise the per-bit fallback to `full()` defaults.
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let attrs = ContainerAttrs {
        special_use: None,
        subscribed: true,
        extra: serde_json::json!({
            "jmap_sort_order": 7,
            "jmap_my_rights": {
                "may_remove_items": false,
                "may_delete": false,
            },
        }),
    };
    let id = mds.create_container(&set, None, "Project", attrs).unwrap();

    let rows = store.list_mailboxes(TEST_ACCOUNT).unwrap();
    let row = rows.iter().find(|r| r.id == id).expect("Project visible");
    assert_eq!(row.sort_order, 7);
    assert!(row.is_subscribed);
    // The two explicitly-false bits override; the rest fall back to
    // the full-rights default.
    assert!(!row.my_rights.may_remove_items);
    assert!(!row.my_rights.may_delete);
    assert!(row.my_rights.may_read_items);
    assert!(row.my_rights.may_add_items);
    assert!(row.my_rights.may_set_seen);
    assert!(row.my_rights.may_set_keywords);
    assert!(row.my_rights.may_create_child);
    assert!(row.my_rights.may_rename);
    assert!(row.my_rights.may_submit);
}

#[test]
fn list_mailboxes_handles_malformed_extra_with_defaults() {
    // Garbage-shaped `extra` (negative number, wrong type) MUST
    // not drop the row from the listing — saturate to defaults
    // and surface the row so a corrupt payload remains visible to
    // operator tooling instead of becoming an invisible ghost.
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);

    // sort_order as a negative value (cannot be u32) → 0.
    let attrs1 = ContainerAttrs {
        special_use: None,
        subscribed: false,
        extra: serde_json::json!({ "jmap_sort_order": -42 }),
    };
    let id1 = mds
        .create_container(&set, None, "BadOrder", attrs1)
        .unwrap();

    // jmap_my_rights as a string instead of an object → default full.
    let attrs2 = ContainerAttrs {
        special_use: None,
        subscribed: false,
        extra: serde_json::json!({ "jmap_my_rights": "not-an-object" }),
    };
    let id2 = mds
        .create_container(&set, None, "BadRights", attrs2)
        .unwrap();

    // extra is an array, not an object → both fields default.
    let attrs3 = ContainerAttrs {
        special_use: None,
        subscribed: false,
        extra: serde_json::json!([1, 2, 3]),
    };
    let id3 = mds
        .create_container(&set, None, "BadShape", attrs3)
        .unwrap();

    let rows = store.list_mailboxes(TEST_ACCOUNT).unwrap();
    let by_id: std::collections::HashMap<_, _> = rows.iter().map(|r| (r.id, r)).collect();

    assert_eq!(by_id[&id1].sort_order, 0);
    assert_eq!(by_id[&id1].my_rights, MailboxRights::full());

    assert_eq!(by_id[&id2].sort_order, 0);
    assert_eq!(by_id[&id2].my_rights, MailboxRights::full());

    assert_eq!(by_id[&id3].sort_order, 0);
    assert_eq!(by_id[&id3].my_rights, MailboxRights::full());
}

#[test]
fn list_mailboxes_subscribed_passthrough_from_attrs() {
    // `is_subscribed` on the JMAP record must equal `attrs.subscribed`
    // verbatim — true → true, false → false.
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);

    let sub_attrs = ContainerAttrs {
        special_use: None,
        subscribed: true,
        extra: serde_json::json!({}),
    };
    let unsub_attrs = ContainerAttrs {
        special_use: None,
        subscribed: false,
        extra: serde_json::json!({}),
    };
    let id_sub = mds
        .create_container(&set, None, "Subbed", sub_attrs)
        .unwrap();
    let id_unsub = mds
        .create_container(&set, None, "Unsubbed", unsub_attrs)
        .unwrap();

    let rows = store.list_mailboxes(TEST_ACCOUNT).unwrap();
    let by_id: std::collections::HashMap<_, _> = rows.iter().map(|r| (r.id, r)).collect();
    assert!(by_id[&id_sub].is_subscribed);
    assert!(!by_id[&id_unsub].is_subscribed);
}

// -------------------- create_mailbox (Task 1.2) ----------------------

fn default_attrs() -> ContainerAttrs {
    ContainerAttrs {
        special_use: None,
        subscribed: false,
        extra: serde_json::json!({}),
    }
}

/// Root-level mailbox is created and shows up in `list_mailboxes`.
#[test]
fn create_mailbox_at_root_succeeds() {
    let (_d, _mds, store) = open_mailstore();
    let id = store
        .create_mailbox(TEST_ACCOUNT, "Inbox", None, default_attrs())
        .unwrap();

    let rows = store.list_mailboxes(TEST_ACCOUNT).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, id);
    assert_eq!(rows[0].name, "Inbox");
    assert!(rows[0].parent.is_none());
}

/// Nested mailbox under an existing parent succeeds and round-trips
/// the parent linkage through `list_mailboxes`.
#[test]
fn create_mailbox_under_existing_parent_succeeds() {
    let (_d, _mds, store) = open_mailstore();
    let parent = store
        .create_mailbox(TEST_ACCOUNT, "Work", None, default_attrs())
        .unwrap();
    let child = store
        .create_mailbox(TEST_ACCOUNT, "Receipts", Some(parent), default_attrs())
        .unwrap();

    let rows = store.list_mailboxes(TEST_ACCOUNT).unwrap();
    let by_id: std::collections::HashMap<_, _> = rows.iter().map(|r| (r.id, r)).collect();
    assert_eq!(by_id[&child].parent, Some(parent));
    assert_eq!(by_id[&child].name, "Receipts");
}

/// Missing parent → fails with the documented `mailbox parent not
/// found` prefix the JMAP `Mailbox/set create` handler keys on.
#[test]
fn create_mailbox_with_missing_parent_fails() {
    let (_d, _mds, store) = open_mailstore();
    let bogus = cosmix_mds::ContainerId(uuid::Uuid::now_v7());
    let err = store
        .create_mailbox(TEST_ACCOUNT, "Orphan", Some(bogus), default_attrs())
        .unwrap_err()
        .to_string();
    assert!(
        err.starts_with("mailbox parent not found"),
        "expected `mailbox parent not found` prefix, got: {err}"
    );
}

/// Reserved-name rule (BLOCKER per Codex review of Task 1.1):
/// `__upload_staging__` collides with `create_blob_ref`'s per-account
/// staging container; accepting it would silently hide the mailbox
/// from `list_mailboxes`.
#[test]
fn create_mailbox_rejects_upload_staging_reserved_name() {
    let (_d, _mds, store) = open_mailstore();
    let err = store
        .create_mailbox(TEST_ACCOUNT, UPLOAD_STAGING_NAME, None, default_attrs())
        .unwrap_err()
        .to_string();
    assert!(
        err.starts_with("reserved name"),
        "expected `reserved name` prefix, got: {err}"
    );

    // Make sure no container was leaked under that name even though
    // the call returned an error early.
    let rows = store.list_mailboxes(TEST_ACCOUNT).unwrap();
    assert!(
        rows.is_empty(),
        "reserved-name reject should not have created any container, got {rows:?}"
    );
}

/// Empty name → fails with the documented `mailbox invalid name`
/// prefix. Guard runs BEFORE `ensure_account_set` so a probe with
/// `""` on a brand-new account does not bootstrap a per-account set
/// as a side effect.
#[test]
fn create_mailbox_rejects_empty_name() {
    let (_d, _mds, store) = open_mailstore();
    let err = store
        .create_mailbox(TEST_ACCOUNT, "", None, default_attrs())
        .unwrap_err()
        .to_string();
    assert!(
        err.starts_with("mailbox invalid name"),
        "expected `mailbox invalid name` prefix, got: {err}"
    );
}

/// Duplicate sibling name fails with the documented prefix.
#[test]
fn create_mailbox_rejects_duplicate_sibling_name() {
    let (_d, _mds, store) = open_mailstore();
    store
        .create_mailbox(TEST_ACCOUNT, "Inbox", None, default_attrs())
        .unwrap();
    let err = store
        .create_mailbox(TEST_ACCOUNT, "Inbox", None, default_attrs())
        .unwrap_err()
        .to_string();
    assert!(
        err.starts_with("mailbox already exists"),
        "expected `mailbox already exists` prefix, got: {err}"
    );
}

// -------------------- rename_mailbox (Task 1.3) ----------------------

/// Plain rename — same parent, new name. The row reflects the new
/// name on the next `list_mailboxes`.
#[test]
fn rename_mailbox_in_place_succeeds() {
    let (_d, _mds, store) = open_mailstore();
    let id = store
        .create_mailbox(TEST_ACCOUNT, "Drafts", None, default_attrs())
        .unwrap();

    store
        .rename_mailbox(TEST_ACCOUNT, id, "Outbox", None)
        .unwrap();

    let rows = store.list_mailboxes(TEST_ACCOUNT).unwrap();
    let by_id: std::collections::HashMap<_, _> = rows.iter().map(|r| (r.id, r)).collect();
    assert_eq!(by_id[&id].name, "Outbox");
    assert!(by_id[&id].parent.is_none());
}

/// Reparent — move a leaf under a different parent.
#[test]
fn rename_mailbox_reparents_under_new_parent() {
    let (_d, _mds, store) = open_mailstore();
    let work = store
        .create_mailbox(TEST_ACCOUNT, "Work", None, default_attrs())
        .unwrap();
    let personal = store
        .create_mailbox(TEST_ACCOUNT, "Personal", None, default_attrs())
        .unwrap();
    let receipts = store
        .create_mailbox(TEST_ACCOUNT, "Receipts", Some(work), default_attrs())
        .unwrap();

    store
        .rename_mailbox(TEST_ACCOUNT, receipts, "Receipts", Some(personal))
        .unwrap();

    let rows = store.list_mailboxes(TEST_ACCOUNT).unwrap();
    let by_id: std::collections::HashMap<_, _> = rows.iter().map(|r| (r.id, r)).collect();
    assert_eq!(by_id[&receipts].parent, Some(personal));
}

/// Renaming to a sibling that already exists fails with the shared
/// `mailbox already exists` prefix — the typed
/// `cosmix_mds::Error::ContainerAlreadyExists` mapping.
#[test]
fn rename_mailbox_to_existing_sibling_name_fails() {
    let (_d, _mds, store) = open_mailstore();
    store
        .create_mailbox(TEST_ACCOUNT, "Inbox", None, default_attrs())
        .unwrap();
    let drafts = store
        .create_mailbox(TEST_ACCOUNT, "Drafts", None, default_attrs())
        .unwrap();

    let err = store
        .rename_mailbox(TEST_ACCOUNT, drafts, "Inbox", None)
        .unwrap_err()
        .to_string();
    assert!(
        err.starts_with("mailbox already exists"),
        "expected `mailbox already exists` prefix, got: {err}"
    );
}

/// Reserved-name rule applies to rename, not just create. A real
/// mailbox renamed to `__upload_staging__` would silently disappear
/// from `list_mailboxes` — same threat as creating one with that
/// name.
#[test]
fn rename_mailbox_rejects_upload_staging_reserved_name() {
    let (_d, _mds, store) = open_mailstore();
    let id = store
        .create_mailbox(TEST_ACCOUNT, "Inbox", None, default_attrs())
        .unwrap();

    let err = store
        .rename_mailbox(TEST_ACCOUNT, id, UPLOAD_STAGING_NAME, None)
        .unwrap_err()
        .to_string();
    assert!(
        err.starts_with("reserved name"),
        "expected `reserved name` prefix, got: {err}"
    );

    // Verify the rename did not partially apply.
    let rows = store.list_mailboxes(TEST_ACCOUNT).unwrap();
    let by_id: std::collections::HashMap<_, _> = rows.iter().map(|r| (r.id, r)).collect();
    assert_eq!(by_id[&id].name, "Inbox");
}

/// Empty name → `mailbox invalid name` prefix, same as create.
#[test]
fn rename_mailbox_rejects_empty_name() {
    let (_d, _mds, store) = open_mailstore();
    let id = store
        .create_mailbox(TEST_ACCOUNT, "Inbox", None, default_attrs())
        .unwrap();

    let err = store
        .rename_mailbox(TEST_ACCOUNT, id, "", None)
        .unwrap_err()
        .to_string();
    assert!(
        err.starts_with("mailbox invalid name"),
        "expected `mailbox invalid name` prefix, got: {err}"
    );
}

/// Renaming a non-existent mailbox surfaces `mailbox not found`.
#[test]
fn rename_mailbox_with_missing_id_fails() {
    let (_d, _mds, store) = open_mailstore();
    // Create one mailbox so the per-account set exists; otherwise
    // `with_set_tx` would surface `SetNotFound` first.
    store
        .create_mailbox(TEST_ACCOUNT, "Inbox", None, default_attrs())
        .unwrap();
    let bogus = cosmix_mds::ContainerId(uuid::Uuid::now_v7());

    let err = store
        .rename_mailbox(TEST_ACCOUNT, bogus, "Stuff", None)
        .unwrap_err()
        .to_string();
    assert!(
        err.starts_with("mailbox not found"),
        "expected `mailbox not found` prefix, got: {err}"
    );
}

/// Reparenting under a non-existent parent surfaces
/// `mailbox parent not found` — same prefix as create_mailbox.
#[test]
fn rename_mailbox_with_missing_parent_fails() {
    let (_d, _mds, store) = open_mailstore();
    let id = store
        .create_mailbox(TEST_ACCOUNT, "Inbox", None, default_attrs())
        .unwrap();
    let bogus = cosmix_mds::ContainerId(uuid::Uuid::now_v7());

    let err = store
        .rename_mailbox(TEST_ACCOUNT, id, "Inbox", Some(bogus))
        .unwrap_err()
        .to_string();
    assert!(
        err.starts_with("mailbox parent not found"),
        "expected `mailbox parent not found` prefix, got: {err}"
    );
}

/// Reparenting under a descendant forms a cycle and is rejected with
/// the `mailbox cycle` prefix.
#[test]
fn rename_mailbox_reparent_into_descendant_fails() {
    let (_d, _mds, store) = open_mailstore();
    let parent = store
        .create_mailbox(TEST_ACCOUNT, "Parent", None, default_attrs())
        .unwrap();
    let child = store
        .create_mailbox(TEST_ACCOUNT, "Child", Some(parent), default_attrs())
        .unwrap();

    let err = store
        .rename_mailbox(TEST_ACCOUNT, parent, "Parent", Some(child))
        .unwrap_err()
        .to_string();
    assert!(
        err.starts_with("mailbox cycle"),
        "expected `mailbox cycle` prefix, got: {err}"
    );
}

/// No-op rename (same name, same parent) round-trips cleanly. The
/// UNIQUE constraint should not fire on an unchanged row, and
/// `list_mailboxes` should observe no shape change.
#[test]
fn rename_mailbox_to_same_name_same_parent_succeeds() {
    let (_d, _mds, store) = open_mailstore();
    let id = store
        .create_mailbox(TEST_ACCOUNT, "Inbox", None, default_attrs())
        .unwrap();

    store
        .rename_mailbox(TEST_ACCOUNT, id, "Inbox", None)
        .unwrap();

    let rows = store.list_mailboxes(TEST_ACCOUNT).unwrap();
    let by_id: std::collections::HashMap<_, _> = rows.iter().map(|r| (r.id, r)).collect();
    assert_eq!(by_id[&id].name, "Inbox");
    assert!(by_id[&id].parent.is_none());
}

/// Reparenting a non-leaf (a container that has its own children)
/// succeeds and the children's parent linkage is unchanged. Documents
/// that cycle detection walks the *new parent's* ancestors, not the
/// moved subtree's descendants.
#[test]
fn rename_mailbox_reparent_subtree_keeps_children_intact() {
    let (_d, _mds, store) = open_mailstore();
    let folders = store
        .create_mailbox(TEST_ACCOUNT, "Folders", None, default_attrs())
        .unwrap();
    let archive = store
        .create_mailbox(TEST_ACCOUNT, "Archive", None, default_attrs())
        .unwrap();
    // Subtree: Folders → Work → Receipts
    let work = store
        .create_mailbox(TEST_ACCOUNT, "Work", Some(folders), default_attrs())
        .unwrap();
    let receipts = store
        .create_mailbox(TEST_ACCOUNT, "Receipts", Some(work), default_attrs())
        .unwrap();

    // Move `Work` (non-leaf) under `Archive`.
    store
        .rename_mailbox(TEST_ACCOUNT, work, "Work", Some(archive))
        .unwrap();

    let rows = store.list_mailboxes(TEST_ACCOUNT).unwrap();
    let by_id: std::collections::HashMap<_, _> = rows.iter().map(|r| (r.id, r)).collect();
    assert_eq!(by_id[&work].parent, Some(archive));
    // Receipts still hangs off Work (which now hangs off Archive).
    assert_eq!(by_id[&receipts].parent, Some(work));
}

/// Source-id wins over parent-id when both are bogus and equal —
/// ambiguity must classify as the source missing, not the parent
/// missing. Regression for the disambiguation MAJOR (Codex review of
/// Task 1.3, round 1).
#[test]
fn rename_mailbox_with_bogus_id_equal_to_bogus_parent_classifies_as_source_missing() {
    let (_d, _mds, store) = open_mailstore();
    // Provision the set so SetNotFound doesn't pre-empt.
    store
        .create_mailbox(TEST_ACCOUNT, "Inbox", None, default_attrs())
        .unwrap();
    let bogus = cosmix_mds::ContainerId(uuid::Uuid::now_v7());

    let err = store
        .rename_mailbox(TEST_ACCOUNT, bogus, "Stuff", Some(bogus))
        .unwrap_err()
        .to_string();
    assert!(
        err.starts_with("mailbox not found"),
        "expected `mailbox not found` (source) prefix, got: {err}"
    );
}

/// Renaming on an unprovisioned account must NOT bootstrap the
/// per-account MDS set. The error must surface as `mailbox not
/// found`, and `list_mailboxes` (which maps `SetNotFound` to empty)
/// must continue to return empty afterwards. Regression for the
/// no-side-effect MAJOR (Codex review of Task 1.3, round 1).
#[test]
fn rename_mailbox_on_unprovisioned_account_does_not_bootstrap_set() {
    const FRESH_ACCOUNT: AccountId = 42;
    let (_d, mds, store) = open_mailstore();
    let bogus = cosmix_mds::ContainerId(uuid::Uuid::now_v7());

    let err = store
        .rename_mailbox(FRESH_ACCOUNT, bogus, "Stuff", None)
        .unwrap_err()
        .to_string();
    assert!(
        err.starts_with("mailbox not found"),
        "expected `mailbox not found` prefix, got: {err}"
    );

    // The set must not have been created as a side effect of the
    // failed rename.
    let set = super::account_id_to_setid(FRESH_ACCOUNT);
    let sets = mds.list_sets().unwrap();
    assert!(
        !sets.iter().any(|s| s == &set),
        "rename on unprovisioned account leaked a set: {sets:?}"
    );
}

/// Renaming a mailbox into itself as parent collapses onto the same
/// `mailbox cycle` prefix — both shapes break the tree.
#[test]
fn rename_mailbox_reparent_under_self_fails() {
    let (_d, _mds, store) = open_mailstore();
    let id = store
        .create_mailbox(TEST_ACCOUNT, "Loop", None, default_attrs())
        .unwrap();

    let err = store
        .rename_mailbox(TEST_ACCOUNT, id, "Loop", Some(id))
        .unwrap_err()
        .to_string();
    assert!(
        err.starts_with("mailbox cycle"),
        "expected `mailbox cycle` prefix, got: {err}"
    );
}

// -------------------- update_mailbox (Task 2.4) ---------------------

/// Patching `sort_order` and `is_subscribed` round-trips through
/// `list_mailboxes` (which decodes the same `attrs` blob).
#[test]
fn update_mailbox_sort_order_and_subscribed_round_trip() {
    let (_d, _mds, store) = open_mailstore();
    let id = store
        .create_mailbox(TEST_ACCOUNT, "Inbox", None, default_attrs())
        .unwrap();

    store
        .update_mailbox(TEST_ACCOUNT, id, None, None, None, Some(7), Some(true))
        .unwrap();

    let rows = store.list_mailboxes(TEST_ACCOUNT).unwrap();
    let row = rows.iter().find(|r| r.id == id).unwrap();
    assert_eq!(row.sort_order, 7);
    assert!(row.is_subscribed);
    assert!(row.role.is_none());
}

/// `role: Some(Some(MailboxRole::Drafts))` sets the role; round-trips
/// back through `list_mailboxes`.
#[test]
fn update_mailbox_sets_role() {
    let (_d, _mds, store) = open_mailstore();
    let id = store
        .create_mailbox(TEST_ACCOUNT, "Drafts", None, default_attrs())
        .unwrap();

    store
        .update_mailbox(
            TEST_ACCOUNT,
            id,
            None,
            None,
            Some(Some(MailboxRole::Drafts)),
            None,
            None,
        )
        .unwrap();

    let rows = store.list_mailboxes(TEST_ACCOUNT).unwrap();
    let row = rows.iter().find(|r| r.id == id).unwrap();
    assert_eq!(row.role, Some(MailboxRole::Drafts));
}

/// `role: Some(None)` clears a previously-set role.
#[test]
fn update_mailbox_clears_role() {
    let (_d, _mds, store) = open_mailstore();
    let id = store
        .create_mailbox(TEST_ACCOUNT, "Drafts", None, inbox_attrs())
        .unwrap();
    // Pre-condition: role is set.
    let rows = store.list_mailboxes(TEST_ACCOUNT).unwrap();
    assert!(rows.iter().find(|r| r.id == id).unwrap().role.is_some());

    store
        .update_mailbox(TEST_ACCOUNT, id, None, None, Some(None), None, None)
        .unwrap();

    let rows = store.list_mailboxes(TEST_ACCOUNT).unwrap();
    assert!(rows.iter().find(|r| r.id == id).unwrap().role.is_none());
}

/// Patching only one field leaves the others alone — sort_order patch
/// must not overwrite `is_subscribed` back to `false`, and vice-versa.
#[test]
fn update_mailbox_partial_patch_preserves_other_fields() {
    let (_d, _mds, store) = open_mailstore();
    let id = store
        .create_mailbox(TEST_ACCOUNT, "Inbox", None, default_attrs())
        .unwrap();

    // Subscribe + sort_order = 5, then patch only sort_order to 9.
    store
        .update_mailbox(TEST_ACCOUNT, id, None, None, None, Some(5), Some(true))
        .unwrap();
    store
        .update_mailbox(TEST_ACCOUNT, id, None, None, None, Some(9), None)
        .unwrap();

    let rows = store.list_mailboxes(TEST_ACCOUNT).unwrap();
    let row = rows.iter().find(|r| r.id == id).unwrap();
    assert_eq!(row.sort_order, 9);
    assert!(
        row.is_subscribed,
        "is_subscribed must survive sort_order patch"
    );
}

/// `sort_order = Some(0)` removes the JMAP key from `extra` (default
/// case) — round-trip back through `jmap_sort_order_from_attrs` is 0.
#[test]
fn update_mailbox_sort_order_zero_drops_key() {
    let (_d, _mds, store) = open_mailstore();
    let id = store
        .create_mailbox(TEST_ACCOUNT, "Inbox", None, default_attrs())
        .unwrap();

    store
        .update_mailbox(TEST_ACCOUNT, id, None, None, None, Some(13), None)
        .unwrap();
    store
        .update_mailbox(TEST_ACCOUNT, id, None, None, None, Some(0), None)
        .unwrap();

    let rows = store.list_mailboxes(TEST_ACCOUNT).unwrap();
    let row = rows.iter().find(|r| r.id == id).unwrap();
    assert_eq!(row.sort_order, 0);
}

/// All-`None` patch is a documented no-op — must not error and must
/// not mutate any field.
#[test]
fn update_mailbox_all_none_is_noop() {
    let (_d, _mds, store) = open_mailstore();
    let id = store
        .create_mailbox(TEST_ACCOUNT, "Inbox", None, default_attrs())
        .unwrap();
    store
        .update_mailbox(
            TEST_ACCOUNT,
            id,
            None,
            None,
            Some(Some(MailboxRole::Inbox)),
            Some(4),
            Some(true),
        )
        .unwrap();

    store
        .update_mailbox(TEST_ACCOUNT, id, None, None, None, None, None)
        .unwrap();

    let rows = store.list_mailboxes(TEST_ACCOUNT).unwrap();
    let row = rows.iter().find(|r| r.id == id).unwrap();
    assert_eq!(row.role, Some(MailboxRole::Inbox));
    assert_eq!(row.sort_order, 4);
    assert!(row.is_subscribed);
}

/// Bogus id → `mailbox not found` prefix. JMAP layer maps this to
/// `notFound`.
#[test]
fn update_mailbox_with_missing_id_fails() {
    let (_d, _mds, store) = open_mailstore();
    store
        .create_mailbox(TEST_ACCOUNT, "Inbox", None, default_attrs())
        .unwrap();
    let bogus = cosmix_mds::ContainerId(uuid::Uuid::now_v7());

    let err = store
        .update_mailbox(TEST_ACCOUNT, bogus, None, None, None, Some(1), None)
        .unwrap_err()
        .to_string();
    assert!(
        err.starts_with("mailbox not found"),
        "expected `mailbox not found` prefix, got: {err}"
    );
}

/// Update on an unprovisioned account does NOT bootstrap the per-set
/// state — same no-side-effect property as `rename_mailbox` /
/// `destroy_mailbox`.
#[test]
fn update_mailbox_on_unprovisioned_account_does_not_bootstrap_set() {
    const FRESH_ACCOUNT: AccountId = 99;
    let (_d, mds, store) = open_mailstore();
    let bogus = cosmix_mds::ContainerId(uuid::Uuid::now_v7());

    let err = store
        .update_mailbox(FRESH_ACCOUNT, bogus, None, None, None, Some(1), None)
        .unwrap_err()
        .to_string();
    assert!(
        err.starts_with("mailbox not found"),
        "expected `mailbox not found` prefix, got: {err}"
    );

    let set = super::account_id_to_setid(FRESH_ACCOUNT);
    let sets = mds.list_sets().unwrap();
    assert!(
        !sets.iter().any(|s| s == &set),
        "update on unprovisioned account leaked a set: {sets:?}"
    );
}

// -------------------- destroy_mailbox (Task 1.4) ---------------------

fn inbox_attrs() -> ContainerAttrs {
    ContainerAttrs {
        special_use: Some("\\Inbox".to_string()),
        subscribed: false,
        extra: serde_json::json!({}),
    }
}

/// Empty mailbox + Reject policy → destroyed; gone from the listing.
#[test]
fn destroy_mailbox_empty_succeeds_under_reject() {
    let (_d, _mds, store) = open_mailstore();
    let id = store
        .create_mailbox(TEST_ACCOUNT, "Tmp", None, default_attrs())
        .unwrap();
    assert_eq!(store.list_mailboxes(TEST_ACCOUNT).unwrap().len(), 1);

    store
        .destroy_mailbox(TEST_ACCOUNT, id, DestroyPolicy::Reject)
        .unwrap();
    assert!(store.list_mailboxes(TEST_ACCOUNT).unwrap().is_empty());
}

/// Non-empty mailbox + Reject policy → fails with "mailbox not empty";
/// the mailbox AND its items remain untouched.
#[test]
fn destroy_mailbox_non_empty_with_reject_fails() {
    let (_d, mds, store) = open_mailstore();
    let id = store
        .create_mailbox(TEST_ACCOUNT, "Bin", None, default_attrs())
        .unwrap();
    let set = account_id_to_setid(TEST_ACCOUNT);
    let item = add_item_in(&mds, &set, id, b"trash");

    let err = store
        .destroy_mailbox(TEST_ACCOUNT, id, DestroyPolicy::Reject)
        .unwrap_err()
        .to_string();
    assert!(
        err.starts_with("mailbox not empty"),
        "expected `mailbox not empty` prefix, got: {err}"
    );
    // Mailbox still listable, item still fetchable.
    assert_eq!(store.list_mailboxes(TEST_ACCOUNT).unwrap().len(), 1);
    mds.fetch_item_meta(&set, &item).unwrap();
}

/// Non-empty mailbox + MoveToInbox → mailbox gone, items now reachable
/// from the per-account `\Inbox` container.
#[test]
fn destroy_mailbox_non_empty_with_move_to_inbox_succeeds_and_emails_reachable() {
    let (_d, mds, store) = open_mailstore();
    let inbox = store
        .create_mailbox(TEST_ACCOUNT, "Inbox", None, inbox_attrs())
        .unwrap();
    let other = store
        .create_mailbox(TEST_ACCOUNT, "Drafts", None, default_attrs())
        .unwrap();
    let set = account_id_to_setid(TEST_ACCOUNT);
    let i1 = add_item_in(&mds, &set, other, b"a");
    let i2 = add_item_in(&mds, &set, other, b"b");

    store
        .destroy_mailbox(TEST_ACCOUNT, other, DestroyPolicy::MoveToInbox)
        .unwrap();

    let rows = store.list_mailboxes(TEST_ACCOUNT).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, inbox);
    assert_eq!(rows[0].total_emails, 2);
    let memb1 = mds.item_memberships(&set, &i1).unwrap();
    let memb2 = mds.item_memberships(&set, &i2).unwrap();
    assert_eq!(memb1.len(), 1);
    assert_eq!(memb1[0].0, inbox);
    assert_eq!(memb2.len(), 1);
    assert_eq!(memb2[0].0, inbox);
}

/// Non-empty mailbox + Delete → mailbox gone, items dropped (last
/// membership released, item rows removed by `remove_membership_in_tx`).
#[test]
fn destroy_mailbox_non_empty_with_delete_succeeds_and_emails_gone() {
    let (_d, mds, store) = open_mailstore();
    let id = store
        .create_mailbox(TEST_ACCOUNT, "Bin", None, default_attrs())
        .unwrap();
    let set = account_id_to_setid(TEST_ACCOUNT);
    let i1 = add_item_in(&mds, &set, id, b"a");
    let i2 = add_item_in(&mds, &set, id, b"b");

    store
        .destroy_mailbox(TEST_ACCOUNT, id, DestroyPolicy::Delete)
        .unwrap();
    assert!(store.list_mailboxes(TEST_ACCOUNT).unwrap().is_empty());
    assert!(mds.fetch_item_meta(&set, &i1).is_err());
    assert!(mds.fetch_item_meta(&set, &i2).is_err());
}

/// Delete policy with a multi-membership item: the item survives in
/// the *other* mailbox, only its membership in the destroyed mailbox
/// goes away.
#[test]
fn destroy_mailbox_delete_preserves_items_with_other_memberships() {
    let (_d, mds, store) = open_mailstore();
    let keep = store
        .create_mailbox(TEST_ACCOUNT, "Keep", None, default_attrs())
        .unwrap();
    let bin = store
        .create_mailbox(TEST_ACCOUNT, "Bin", None, default_attrs())
        .unwrap();
    let set = account_id_to_setid(TEST_ACCOUNT);
    let (hash, _) = put_blob(&mds, b"shared");
    let item = mds
        .add_item(
            &set,
            &hash,
            &[
                Membership {
                    container: keep,
                    flags: Flags(0),
                    added_at: 0,
                },
                Membership {
                    container: bin,
                    flags: Flags(0),
                    added_at: 0,
                },
            ],
        )
        .unwrap()
        .item_id;

    store
        .destroy_mailbox(TEST_ACCOUNT, bin, DestroyPolicy::Delete)
        .unwrap();
    let memb = mds.item_memberships(&set, &item).unwrap();
    assert_eq!(memb.len(), 1);
    assert_eq!(memb[0].0, keep);
    mds.fetch_item_meta(&set, &item).unwrap();
}

/// MoveToInbox where an item is already in inbox AND the source: the
/// duplicate membership in inbox stays; only the source membership
/// goes (no `move_item` "already in destination" failure).
#[test]
fn destroy_mailbox_move_to_inbox_handles_duplicate_membership() {
    let (_d, mds, store) = open_mailstore();
    let inbox = store
        .create_mailbox(TEST_ACCOUNT, "Inbox", None, inbox_attrs())
        .unwrap();
    let other = store
        .create_mailbox(TEST_ACCOUNT, "Other", None, default_attrs())
        .unwrap();
    let set = account_id_to_setid(TEST_ACCOUNT);
    let (hash, _) = put_blob(&mds, b"dup");
    let item = mds
        .add_item(
            &set,
            &hash,
            &[
                Membership {
                    container: inbox,
                    flags: Flags(0),
                    added_at: 0,
                },
                Membership {
                    container: other,
                    flags: Flags(0),
                    added_at: 0,
                },
            ],
        )
        .unwrap()
        .item_id;

    store
        .destroy_mailbox(TEST_ACCOUNT, other, DestroyPolicy::MoveToInbox)
        .unwrap();
    let memb = mds.item_memberships(&set, &item).unwrap();
    assert_eq!(memb.len(), 1);
    assert_eq!(memb[0].0, inbox);
}

/// MoveToInbox + no `\Inbox` configured → fails with prefix
/// "mailbox has no inbox"; the source mailbox is left intact.
#[test]
fn destroy_mailbox_move_to_inbox_with_no_inbox_fails() {
    let (_d, mds, store) = open_mailstore();
    let id = store
        .create_mailbox(TEST_ACCOUNT, "Stuff", None, default_attrs())
        .unwrap();
    let set = account_id_to_setid(TEST_ACCOUNT);
    let item = add_item_in(&mds, &set, id, b"x");

    let err = store
        .destroy_mailbox(TEST_ACCOUNT, id, DestroyPolicy::MoveToInbox)
        .unwrap_err()
        .to_string();
    assert!(
        err.starts_with("mailbox has no inbox"),
        "expected `mailbox has no inbox` prefix, got: {err}"
    );
    assert_eq!(store.list_mailboxes(TEST_ACCOUNT).unwrap().len(), 1);
    mds.fetch_item_meta(&set, &item).unwrap();
}

/// MoveToInbox where the destroy target *is* the inbox now fires
/// the role-protection guard (RFC 8621 §2.5) before the
/// conflicting-policy guard inside `MoveToInbox`. The
/// conflicting-policy branch is preserved as defense in depth in
/// case the inbox is ever provisioned without the `\Inbox` role.
#[test]
fn destroy_mailbox_move_to_inbox_when_id_is_inbox_fails() {
    let (_d, _mds, store) = open_mailstore();
    let inbox = store
        .create_mailbox(TEST_ACCOUNT, "Inbox", None, inbox_attrs())
        .unwrap();

    let err = store
        .destroy_mailbox(TEST_ACCOUNT, inbox, DestroyPolicy::MoveToInbox)
        .unwrap_err()
        .to_string();
    assert!(
        err.starts_with("mailbox has role"),
        "expected `mailbox has role` prefix, got: {err}"
    );
    assert_eq!(store.list_mailboxes(TEST_ACCOUNT).unwrap().len(), 1);
}

/// Destroying a mailbox with surviving child mailboxes is rejected via
/// the MDS `ON DELETE RESTRICT` rule on `container.parent_id`.
#[test]
fn destroy_mailbox_with_children_fails() {
    let (_d, _mds, store) = open_mailstore();
    let parent = store
        .create_mailbox(TEST_ACCOUNT, "Parent", None, default_attrs())
        .unwrap();
    let _child = store
        .create_mailbox(TEST_ACCOUNT, "Child", Some(parent), default_attrs())
        .unwrap();

    let err = store
        .destroy_mailbox(TEST_ACCOUNT, parent, DestroyPolicy::Reject)
        .unwrap_err()
        .to_string();
    assert!(
        err.starts_with("mailbox has children"),
        "expected `mailbox has children` prefix, got: {err}"
    );
    assert_eq!(store.list_mailboxes(TEST_ACCOUNT).unwrap().len(), 2);
}

/// Bogus id → "mailbox not found" with no observable side effect.
#[test]
fn destroy_mailbox_with_missing_id_fails() {
    let (_d, _mds, store) = open_mailstore();
    let _existing = store
        .create_mailbox(TEST_ACCOUNT, "Real", None, default_attrs())
        .unwrap();
    let bogus = cosmix_mds::ContainerId(uuid::Uuid::now_v7());

    let err = store
        .destroy_mailbox(TEST_ACCOUNT, bogus, DestroyPolicy::Reject)
        .unwrap_err()
        .to_string();
    assert!(
        err.starts_with("mailbox not found"),
        "expected `mailbox not found` prefix, got: {err}"
    );
    assert_eq!(store.list_mailboxes(TEST_ACCOUNT).unwrap().len(), 1);
}

/// Same bootstrap-avoidance invariant as `rename_mailbox`: destroy on
/// an unprovisioned account surfaces "mailbox not found" without
/// creating the account's MDS set as a side effect.
#[test]
fn destroy_mailbox_on_unprovisioned_account_does_not_bootstrap_set() {
    let (_d, mds, store) = open_mailstore();
    assert!(mds.list_sets().unwrap().is_empty());
    let bogus = cosmix_mds::ContainerId(uuid::Uuid::now_v7());

    let err = store
        .destroy_mailbox(TEST_ACCOUNT, bogus, DestroyPolicy::Delete)
        .unwrap_err()
        .to_string();
    assert!(
        err.starts_with("mailbox not found"),
        "expected `mailbox not found` prefix, got: {err}"
    );
    assert!(mds.list_sets().unwrap().is_empty());
}

/// Defensive: refuse to destroy the per-account `__upload_staging__`
/// container by id even if a caller somehow obtained it. Otherwise
/// `create_blob_ref` would silently re-provision it on next call,
/// orphaning any in-flight blob_refs whose `temp_item_id` lived in
/// the old container row.
#[test]
fn destroy_mailbox_rejects_upload_staging_container() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let (hash, size) = put_blob(&mds, b"upload");
    let _bid = store
        .create_blob_ref(TEST_ACCOUNT, hash, size, i64::MAX)
        .unwrap();
    let staging_id = mds
        .list_containers(&set)
        .unwrap()
        .into_iter()
        .find(|c| c.name == UPLOAD_STAGING_NAME)
        .map(|c| c.id)
        .expect("upload-staging container should exist after create_blob_ref");

    let err = store
        .destroy_mailbox(TEST_ACCOUNT, staging_id, DestroyPolicy::Reject)
        .unwrap_err()
        .to_string();
    assert!(
        err.starts_with("reserved container"),
        "expected `reserved container` prefix, got: {err}"
    );
}

#[test]
fn destroy_mailbox_with_role_rejected_under_reject() {
    // RFC 8621 §2.5 role protection: even an empty role mailbox
    // refuses destroy under the default policy. Without this
    // guard, an empty `\Inbox` would pass the count check.
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    let err = store
        .destroy_mailbox(TEST_ACCOUNT, inbox, DestroyPolicy::Reject)
        .unwrap_err()
        .to_string();
    assert!(
        err.starts_with("mailbox has role"),
        "expected `mailbox has role` prefix, got: {err}"
    );
}

#[test]
fn destroy_mailbox_with_role_rejected_under_delete() {
    // The role guard runs *before* the policy branch. Without
    // it, `onDestroyRemoveEmails: true` (DestroyPolicy::Delete)
    // would otherwise reach `remove_membership` and silently
    // tear down the inbox along with its mail.
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));
    let _item = add_item_in(&mds, &set, inbox, b"hello");

    let err = store
        .destroy_mailbox(TEST_ACCOUNT, inbox, DestroyPolicy::Delete)
        .unwrap_err()
        .to_string();
    assert!(
        err.starts_with("mailbox has role"),
        "expected `mailbox has role` prefix, got: {err}"
    );
}

#[test]
fn destroy_mailbox_after_clearing_role_succeeds() {
    // The advertised escape hatch: clear the role via update,
    // then destroy. Confirms the guard reads the live attrs row,
    // not a snapshot.
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let trash = mk_container(&mds, &set, "Trash", Some("\\Trash"));

    store
        .update_mailbox(TEST_ACCOUNT, trash, None, None, Some(None), None, None)
        .unwrap();
    store
        .destroy_mailbox(TEST_ACCOUNT, trash, DestroyPolicy::Reject)
        .expect("destroy after role-clear should succeed");
    assert!(
        mds.list_containers(&set)
            .unwrap()
            .into_iter()
            .all(|c| c.id != trash),
        "trash container should be gone"
    );
}

// --- Task 1.5: list_emails_in_mailbox ---------------------------------

/// Force `item.received_at` for a specific item id so sort-by-time
/// tests aren't subject to `now_ms()` clock granularity (multiple
/// `add_item` calls in the same millisecond would tie). Direct SQL,
/// keyed off the per-set tx so the write is durable.
fn set_received_at(mds: &SqliteCasMds, set: &SetId, item: &ItemId, ts: i64) {
    mds.with_set_tx(set, |tx| {
        tx.tx()
            .execute(
                "UPDATE item SET received_at = ?2 WHERE id = ?1",
                params![item.0.to_string(), ts],
            )
            .unwrap();
        Ok(())
    })
    .unwrap();
}

#[test]
fn list_emails_empty_mailbox_returns_empty_vec() {
    let (_d, _mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&_mds, &set, "Inbox", Some("\\Inbox"));

    let out = store
        .list_emails_in_mailbox(TEST_ACCOUNT, inbox, ListOpts::default())
        .unwrap();
    assert!(out.is_empty());
}

#[test]
fn list_emails_returns_all_items_with_correct_fields() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    let id1 = add_item_in(&mds, &set, inbox, b"hello world");
    let id2 = add_item_in(&mds, &set, inbox, b"second email");

    let out = store
        .list_emails_in_mailbox(TEST_ACCOUNT, inbox, ListOpts::default())
        .unwrap();
    assert_eq!(out.len(), 2);

    // Default sort (ReceivedAt, with seq tie-break) preserves add
    // order when add_item calls happen in sequence: each item gets
    // a successive `seq` and (typically) the same or a monotonic
    // `received_at`.
    let ids: Vec<ItemId> = out.iter().map(|h| h.id).collect();
    assert_eq!(ids, vec![id1, id2]);

    let h0 = &out[0];
    assert_eq!(h0.id, id1);
    assert_eq!(h0.keywords, Flags(0));
    // received_at populated by Mds::add_item from now_ms() — just
    // sanity-check it's non-zero.
    assert!(h0.received_at > 0);
}

#[test]
fn list_emails_with_limit_caps_result() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    for i in 0..12 {
        add_item_in(&mds, &set, inbox, format!("email {i}").as_bytes());
    }

    let out = store
        .list_emails_in_mailbox(
            TEST_ACCOUNT,
            inbox,
            ListOpts {
                limit: 10,
                offset: 0,
                sort_by: SortKey::Seq,
            },
        )
        .unwrap();
    assert_eq!(out.len(), 10);
}

#[test]
fn list_emails_with_offset_skips_prefix() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    let ids: Vec<ItemId> = (0..5)
        .map(|i| add_item_in(&mds, &set, inbox, format!("email {i}").as_bytes()))
        .collect();

    let out = store
        .list_emails_in_mailbox(
            TEST_ACCOUNT,
            inbox,
            ListOpts {
                limit: u32::MAX,
                offset: 3,
                sort_by: SortKey::Seq,
            },
        )
        .unwrap();
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].id, ids[3]);
    assert_eq!(out[1].id, ids[4]);
}

#[test]
fn list_emails_with_limit_and_offset_pages() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    let ids: Vec<ItemId> = (0..7)
        .map(|i| add_item_in(&mds, &set, inbox, format!("email {i}").as_bytes()))
        .collect();

    let out = store
        .list_emails_in_mailbox(
            TEST_ACCOUNT,
            inbox,
            ListOpts {
                limit: 3,
                offset: 2,
                sort_by: SortKey::Seq,
            },
        )
        .unwrap();
    assert_eq!(out.len(), 3);
    assert_eq!(out[0].id, ids[2]);
    assert_eq!(out[2].id, ids[4]);
}

#[test]
fn list_emails_with_limit_zero_returns_empty() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    add_item_in(&mds, &set, inbox, b"will not be returned");

    let out = store
        .list_emails_in_mailbox(
            TEST_ACCOUNT,
            inbox,
            ListOpts {
                limit: 0,
                offset: 0,
                sort_by: SortKey::Seq,
            },
        )
        .unwrap();
    assert!(out.is_empty());
}

#[test]
fn list_emails_with_offset_past_end_returns_empty() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    add_item_in(&mds, &set, inbox, b"only one");

    let out = store
        .list_emails_in_mailbox(
            TEST_ACCOUNT,
            inbox,
            ListOpts {
                limit: 10,
                offset: 99,
                sort_by: SortKey::Seq,
            },
        )
        .unwrap();
    assert!(out.is_empty());
}

#[test]
fn list_emails_sort_by_received_at_orders_chronologically_with_seq_tiebreak() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    // Add three items, then *manually* set their received_at so the
    // chronological order is the reverse of insertion order. Two
    // share a timestamp to exercise the seq tie-break path.
    let id_a = add_item_in(&mds, &set, inbox, b"a");
    let id_b = add_item_in(&mds, &set, inbox, b"b");
    let id_c = add_item_in(&mds, &set, inbox, b"c");

    set_received_at(&mds, &set, &id_a, 3000);
    set_received_at(&mds, &set, &id_b, 1000);
    set_received_at(&mds, &set, &id_c, 1000);

    let out = store
        .list_emails_in_mailbox(
            TEST_ACCOUNT,
            inbox,
            ListOpts {
                limit: u32::MAX,
                offset: 0,
                sort_by: SortKey::ReceivedAt,
            },
        )
        .unwrap();
    assert_eq!(out.len(), 3);
    // 1000@b (lower seq), 1000@c (higher seq), 3000@a.
    assert_eq!(out[0].id, id_b);
    assert_eq!(out[1].id, id_c);
    assert_eq!(out[2].id, id_a);
}

#[test]
fn list_emails_limit_zero_on_missing_mailbox_still_errors() {
    // The existence check must run *before* the `limit == 0`
    // early-return so JMAP "count probe" callers get a distinct
    // `mailbox not found` rather than a misleading empty success.
    let (_d, _mds, store) = open_mailstore();
    let _set = provision(&store);

    let bogus = ContainerId(uuid::Uuid::new_v4());
    let err = store
        .list_emails_in_mailbox(
            TEST_ACCOUNT,
            bogus,
            ListOpts {
                limit: 0,
                offset: 0,
                sort_by: SortKey::Seq,
            },
        )
        .unwrap_err()
        .to_string();
    assert!(
        err.starts_with("mailbox not found"),
        "expected `mailbox not found` prefix even at limit=0, got: {err}"
    );
}

#[test]
fn list_emails_on_missing_mailbox_errors_with_mailbox_not_found() {
    let (_d, _mds, store) = open_mailstore();
    let _set = provision(&store);

    let bogus = ContainerId(uuid::Uuid::new_v4());
    let err = store
        .list_emails_in_mailbox(TEST_ACCOUNT, bogus, ListOpts::default())
        .unwrap_err()
        .to_string();
    assert!(
        err.starts_with("mailbox not found"),
        "expected `mailbox not found` prefix, got: {err}"
    );
}

#[test]
fn list_emails_on_unprovisioned_account_does_not_bootstrap_set() {
    let (_d, mds, store) = open_mailstore();
    let bogus = ContainerId(uuid::Uuid::new_v4());

    let err = store
        .list_emails_in_mailbox(TEST_ACCOUNT, bogus, ListOpts::default())
        .unwrap_err()
        .to_string();
    assert!(
        err.starts_with("mailbox not found"),
        "expected `mailbox not found` prefix, got: {err}"
    );
    // No empty per-account set should have been created on the
    // failed read path — same invariant as rename_mailbox /
    // destroy_mailbox.
    assert!(
        mds.list_sets().unwrap().is_empty(),
        "list_emails_in_mailbox failure must not bootstrap a per-account set"
    );
}

#[test]
fn list_emails_filters_to_requested_mailbox_only() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));
    let other = mk_container(&mds, &set, "Other", None);

    let in_inbox = add_item_in(&mds, &set, inbox, b"inbox msg");
    let _in_other = add_item_in(&mds, &set, other, b"other msg");

    let out = store
        .list_emails_in_mailbox(TEST_ACCOUNT, inbox, ListOpts::default())
        .unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].id, in_inbox);
}

#[test]
fn list_emails_carries_blob_hash_and_keywords_through() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    let payload = b"keyword carrier test";
    let (hash, _) = put_blob(&mds, payload);
    let m = Membership {
        container: inbox,
        flags: Flags(0b0001), // \Seen-ish bit, opaque to MailStore
        added_at: 0,
    };
    let id = mds.add_item(&set, &hash, &[m]).unwrap().item_id;

    let out = store
        .list_emails_in_mailbox(TEST_ACCOUNT, inbox, ListOpts::default())
        .unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].id, id);
    assert_eq!(out[0].blob_hash, hash);
    assert_eq!(out[0].keywords, Flags(0b0001));
}

// -------------------- get_email --------------------

/// Insert a `mail_envelopes` row directly. `MailStore::create_email`
/// (Task 1.7) is the production write path; this helper drives the
/// envelope sidecar from a test until that lands so Task 1.6's reads
/// can be exercised end-to-end against the v1.2 schema.
fn insert_envelope(mds: &SqliteCasMds, set: &SetId, item: &ItemId, env: &EmailEnvelope) {
    let to_json = serde_json::to_string(&env.to).unwrap();
    let cc_json = serde_json::to_string(&env.cc).unwrap();
    let bcc_json = serde_json::to_string(&env.bcc).unwrap();
    let reply_to_json = serde_json::to_string(&env.reply_to).unwrap();
    mds.with_set_tx(set, |tx| {
        tx.tx()
            .execute(
                "INSERT INTO mail_envelopes \
                 (item_id, from_addr, to_addrs, cc_addrs, bcc_addrs, \
                  reply_to_addrs, subject, date_ts, message_id) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    item.0.to_string(),
                    env.from,
                    to_json,
                    cc_json,
                    bcc_json,
                    reply_to_json,
                    env.subject,
                    env.date,
                    env.message_id,
                ],
            )
            .unwrap();
        Ok(())
    })
    .unwrap();
}

fn sample_envelope() -> EmailEnvelope {
    EmailEnvelope {
        from: "alice@example.com".into(),
        to: vec!["bob@example.com".into(), "carol@example.com".into()],
        cc: Vec::new(),
        bcc: Vec::new(),
        reply_to: Vec::new(),
        subject: "lunch?".into(),
        date: 1_700_000_000,
        message_id: Some("<msg-1@example.com>".into()),
    }
}

#[test]
fn get_email_returns_record_with_envelope_meta_keywords_mailboxes() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    let payload = b"hello world";
    let (hash, _) = put_blob(&mds, payload);
    let m = Membership {
        container: inbox,
        flags: Flags(0b0010),
        added_at: 0,
    };
    let id = mds.add_item(&set, &hash, &[m]).unwrap().item_id;

    let env = sample_envelope();
    insert_envelope(&mds, &set, &id, &env);

    let rec: EmailRecord = store.get_email(TEST_ACCOUNT, id).unwrap();
    assert_eq!(rec.id, id);
    assert_eq!(rec.blob_hash, hash);
    assert_eq!(rec.size_bytes, payload.len() as u64);
    assert_eq!(rec.envelope, env);
    assert_eq!(rec.keywords, Flags(0b0010));
    assert_eq!(rec.mailbox_ids, vec![inbox]);
}

#[test]
fn get_email_keywords_are_union_across_memberships() {
    // RFC 8621 §4.1.4: JMAP `keywords` is the union of per-mailbox
    // flags. The MailStore adapter is responsible for that merge —
    // verify a multi-membership item ORs flag bits across all rows.
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));
    let archive = mk_container(&mds, &set, "Archive", Some("\\Archive"));

    let (hash, _) = put_blob(&mds, b"multi-membership");
    let memberships = [
        Membership {
            container: inbox,
            flags: Flags(0b0001),
            added_at: 0,
        },
        Membership {
            container: archive,
            flags: Flags(0b0100),
            added_at: 0,
        },
    ];
    let id = mds.add_item(&set, &hash, &memberships).unwrap().item_id;

    insert_envelope(&mds, &set, &id, &sample_envelope());

    let rec = store.get_email(TEST_ACCOUNT, id).unwrap();
    assert_eq!(rec.keywords, Flags(0b0101));
    assert_eq!(rec.mailbox_ids.len(), 2);
    assert!(rec.mailbox_ids.contains(&inbox));
    assert!(rec.mailbox_ids.contains(&archive));
}

#[test]
fn get_email_missing_item_errors_with_email_not_found() {
    let (_d, _mds, store) = open_mailstore();
    let _set = provision(&store);
    let bogus = ItemId(uuid::Uuid::new_v4());
    let err = store.get_email(TEST_ACCOUNT, bogus).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.starts_with("email not found"), "got: {msg}");
}

#[test]
fn get_email_unprovisioned_account_errors_with_email_not_found() {
    // No `provision` call: the account's set has not been bootstrapped.
    // get_email must not bootstrap on an error path; SetNotFound maps to
    // "email not found" because the item cannot exist if its set
    // doesn't.
    let (_d, _mds, store) = open_mailstore();
    let bogus = ItemId(uuid::Uuid::new_v4());
    let err = store.get_email(TEST_ACCOUNT, bogus).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.starts_with("email not found"), "got: {msg}");
}

#[test]
fn get_email_item_without_envelope_row_errors_with_envelope_missing() {
    // Phase A transient: items that pre-date Task 1.7's `create_email`
    // write path lack a `mail_envelopes` row. Surfacing a distinct
    // sentinel ("email envelope missing") lets the JMAP layer
    // synthesise a skeleton envelope at serialise time once that path
    // lands, rather than conflating with `notFound`.
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));
    let id = add_item_in(&mds, &set, inbox, b"no-envelope");

    let err = store.get_email(TEST_ACCOUNT, id).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.starts_with("email envelope missing"), "got: {msg}");
}

#[test]
fn get_email_envelope_with_no_to_or_message_id_round_trips() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));
    let id = add_item_in(&mds, &set, inbox, b"sparse");

    let env = EmailEnvelope {
        from: "robot@example.com".into(),
        to: vec![],
        cc: Vec::new(),
        bcc: Vec::new(),
        reply_to: Vec::new(),
        subject: String::new(),
        date: 0,
        message_id: None,
    };
    insert_envelope(&mds, &set, &id, &env);

    let rec = store.get_email(TEST_ACCOUNT, id).unwrap();
    assert_eq!(rec.envelope, env);
}

// -------------------- create_email --------------------

#[test]
fn create_email_happy_path_visible_via_get_email() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    let payload = b"freshly delivered";
    let (hash, size) = put_blob(&mds, payload);
    let env = sample_envelope();
    let received_at: i64 = 1_730_000_000_000;

    let (id, _uid) = store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            hash,
            env.clone(),
            &[],
            Flags(0b0001),
            Tags::new(),
            received_at,
        )
        .unwrap();

    let rec = store.get_email(TEST_ACCOUNT, id).unwrap();
    assert_eq!(rec.id, id);
    assert_eq!(rec.blob_hash, hash);
    assert_eq!(rec.size_bytes, size);
    assert_eq!(rec.received_at, received_at);
    assert_eq!(rec.envelope, env);
    assert_eq!(rec.keywords, Flags(0b0001));
    assert_eq!(rec.mailbox_ids, vec![inbox]);
}

// ---------- v1.8 item-keyed sidecar reap (FK cascade + trigger) ----------

/// `destroy_mailbox(.., Delete)` on the sole mailbox drops the item,
/// which must reap ALL three item-keyed sidecars: mail_threads +
/// mail_search via the v1.8 FK cascade / AFTER DELETE trigger,
/// mail_retrain_outbox via the FK cascade.
#[test]
fn destroy_mailbox_delete_reaps_all_item_sidecars() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    // Plain (un-roled) mailbox so destroy_mailbox(.., Delete) is allowed.
    let bin = store
        .create_mailbox(TEST_ACCOUNT, "Bin", None, default_attrs())
        .unwrap();

    // create_email populates mail_envelopes + mail_threads + mail_search.
    let (hash, _) = put_blob(&mds, b"reap me");
    let (id, _uid) = store
        .create_email(
            TEST_ACCOUNT,
            bin,
            hash,
            sample_envelope(),
            &[],
            Flags(0),
            Tags::new(),
            1_730_000_000_000,
        )
        .unwrap();
    // Enqueue a retrain-outbox row for the same item (direct insert —
    // a \Junk move just to enqueue would be noisier).
    insert_retrain_outbox(&mds, &set, &id, "stamp-reap", "junk");

    // Pre-condition: all three sidecars carry a row for this item.
    assert_eq!(count_sidecar_for_item(&mds, &set, "mail_threads", &id), 1);
    assert_eq!(count_sidecar_for_item(&mds, &set, "mail_search", &id), 1);
    assert_eq!(
        count_sidecar_for_item(&mds, &set, "mail_retrain_outbox", &id),
        1
    );

    store
        .destroy_mailbox(TEST_ACCOUNT, bin, DestroyPolicy::Delete)
        .unwrap();

    // Item gone, all three sidecars reaped.
    assert!(mds.fetch_item_meta(&set, &id).is_err());
    assert_eq!(count_sidecar_for_item(&mds, &set, "mail_threads", &id), 0);
    assert_eq!(count_sidecar_for_item(&mds, &set, "mail_search", &id), 0);
    assert_eq!(
        count_sidecar_for_item(&mds, &set, "mail_retrain_outbox", &id),
        0
    );
}

/// `delete_email` walks every membership; the last `remove_membership`
/// drops the item, reaping all three item-keyed sidecars.
#[test]
fn delete_email_reaps_all_item_sidecars() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    let (hash, _) = put_blob(&mds, b"delete me");
    let (id, _uid) = store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            hash,
            sample_envelope(),
            &[],
            Flags(0),
            Tags::new(),
            1_730_000_000_000,
        )
        .unwrap();
    insert_retrain_outbox(&mds, &set, &id, "stamp-del", "ham");

    assert_eq!(count_sidecar_for_item(&mds, &set, "mail_threads", &id), 1);
    assert_eq!(count_sidecar_for_item(&mds, &set, "mail_search", &id), 1);
    assert_eq!(
        count_sidecar_for_item(&mds, &set, "mail_retrain_outbox", &id),
        1
    );

    store.delete_email(TEST_ACCOUNT, id).unwrap();

    assert!(mds.fetch_item_meta(&set, &id).is_err());
    assert_eq!(count_sidecar_for_item(&mds, &set, "mail_threads", &id), 0);
    assert_eq!(count_sidecar_for_item(&mds, &set, "mail_search", &id), 0);
    assert_eq!(
        count_sidecar_for_item(&mds, &set, "mail_retrain_outbox", &id),
        0
    );
}

/// Inverse: a multi-homed item (in two mailboxes). Destroying ONE
/// mailbox removes only that membership — the item survives and ALL
/// three sidecars are PRESERVED (the cascade fires only on the final
/// `DELETE FROM item`, which does not happen here).
#[test]
fn destroy_one_of_two_mailboxes_preserves_item_sidecars() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    // Two plain (un-roled) mailboxes so destroy(.., Delete) is allowed.
    let bin = store
        .create_mailbox(TEST_ACCOUNT, "Bin", None, default_attrs())
        .unwrap();
    let keep = store
        .create_mailbox(TEST_ACCOUNT, "Keep", None, default_attrs())
        .unwrap();

    // Create the email in bin, then also home it in keep.
    let (hash, _) = put_blob(&mds, b"multi-homed");
    let (id, _uid) = store
        .create_email(
            TEST_ACCOUNT,
            bin,
            hash,
            sample_envelope(),
            &[],
            Flags(0),
            Tags::new(),
            1_730_000_000_000,
        )
        .unwrap();
    mds.with_set_tx(&set, |tx| {
        tx.add_membership(&id, &keep, Flags(0))?;
        Ok(())
    })
    .unwrap();
    insert_retrain_outbox(&mds, &set, &id, "stamp-multi", "junk");

    // Destroy only the bin membership.
    store
        .destroy_mailbox(TEST_ACCOUNT, bin, DestroyPolicy::Delete)
        .unwrap();

    // Item survives (still in keep); all three sidecars preserved.
    mds.fetch_item_meta(&set, &id).unwrap();
    let memb = mds.item_memberships(&set, &id).unwrap();
    assert_eq!(memb.len(), 1);
    assert_eq!(memb[0].0, keep);
    assert_eq!(count_sidecar_for_item(&mds, &set, "mail_threads", &id), 1);
    assert_eq!(count_sidecar_for_item(&mds, &set, "mail_search", &id), 1);
    assert_eq!(
        count_sidecar_for_item(&mds, &set, "mail_retrain_outbox", &id),
        1
    );
}

#[test]
fn create_email_unknown_blob_errors_with_blob_not_found() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    // Hash some bytes but never `put_blob` — the hash is well-formed
    // but no file exists for it in CAS.
    let phantom = cosmix_mds::blob::hash_bytes(b"bytes that were never put");
    let err = store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            phantom,
            sample_envelope(),
            &[],
            Flags(0),
            Tags::new(),
            0,
        )
        .unwrap_err();
    let msg = format!("{err}");
    assert!(msg.starts_with("blob not found"), "got: {msg}");
}

#[test]
fn create_email_unknown_mailbox_errors_with_mailbox_not_found() {
    let (_d, mds, store) = open_mailstore();
    let _set = provision(&store);

    let (hash, _) = put_blob(&mds, b"orphan delivery");
    // ContainerId that was never created in this set.
    let bogus_mailbox = ContainerId(uuid::Uuid::new_v4());

    let err = store
        .create_email(
            TEST_ACCOUNT,
            bogus_mailbox,
            hash,
            sample_envelope(),
            &[],
            Flags(0),
            Tags::new(),
            0,
        )
        .unwrap_err();
    let msg = format!("{err}");
    assert!(msg.starts_with("mailbox not found"), "got: {msg}");
}

#[test]
fn create_email_unprovisioned_account_errors_with_mailbox_not_found() {
    // No `provision` call: the account's set has never been
    // bootstrapped. SetNotFound surfaces as "mailbox not found"
    // because no mailbox can exist if its set doesn't.
    let (_d, mds, store) = open_mailstore();
    let (hash, _) = put_blob(&mds, b"into the void");
    let bogus_mailbox = ContainerId(uuid::Uuid::new_v4());

    let err = store
        .create_email(
            TEST_ACCOUNT,
            bogus_mailbox,
            hash,
            sample_envelope(),
            &[],
            Flags(0),
            Tags::new(),
            0,
        )
        .unwrap_err();
    let msg = format!("{err}");
    assert!(msg.starts_with("mailbox not found"), "got: {msg}");
}

#[test]
fn create_email_received_at_is_independent_of_envelope_date() {
    // IMAP `INTERNALDATE` / JMAP `receivedAt` is the server-received
    // time; `envelope.date` is the message's `Date:` header. They can
    // legitimately differ — verify the impl preserves both
    // independently rather than forcing them to a single timestamp.
    //
    // Unit reminder: `envelope.date` is unix-seconds (RFC 5322
    // origination time, see `EmailEnvelope::date` doc), `received_at`
    // is unix-milliseconds (matches `item.received_at` written by
    // mds via `now_ms()`). The `assert_ne!` below is therefore not
    // just a value check — these fields are stored in different
    // units, and a future change collapsing them onto a single column
    // would need to reconcile that.
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    let (hash, _) = put_blob(&mds, b"delayed delivery");
    let env = EmailEnvelope {
        from: "remote@example.com".into(),
        to: vec!["local@example.com".into()],
        cc: Vec::new(),
        bcc: Vec::new(),
        reply_to: Vec::new(),
        subject: "delayed".into(),
        date: 1_700_000_000, // Date: header, unix-seconds
        message_id: Some("<delayed@example.com>".into()),
    };
    let received_at: i64 = 1_730_000_000_000; // server received, unix-ms

    let (id, _uid) = store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            hash,
            env.clone(),
            &[],
            Flags(0),
            Tags::new(),
            received_at,
        )
        .unwrap();

    let rec = store.get_email(TEST_ACCOUNT, id).unwrap();
    assert_eq!(rec.received_at, received_at);
    assert_eq!(rec.envelope.date, env.date);
    assert_ne!(rec.received_at, rec.envelope.date);
}

#[test]
fn create_email_twice_for_same_blob_yields_distinct_item_ids() {
    // Dedup is at the *blob* layer, not the *item* layer: the same
    // bytes can back many items (different deliveries, different
    // mailboxes, different keywords). Each `create_email` call must
    // mint a fresh ItemId.
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    let (hash, _) = put_blob(&mds, b"shared bytes, distinct deliveries");
    let env = sample_envelope();

    let (id1, _uid1) = store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            hash,
            env.clone(),
            &[],
            Flags(0),
            Tags::new(),
            1_700_000_000_000,
        )
        .unwrap();
    let (id2, _uid2) = store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            hash,
            env.clone(),
            &[],
            Flags(0),
            Tags::new(),
            1_700_000_001_000,
        )
        .unwrap();
    assert_ne!(
        id1, id2,
        "duplicate create_email must produce distinct ItemIds"
    );

    // Both items resolve via get_email to the same blob.
    let r1 = store.get_email(TEST_ACCOUNT, id1).unwrap();
    let r2 = store.get_email(TEST_ACCOUNT, id2).unwrap();
    assert_eq!(r1.blob_hash, hash);
    assert_eq!(r2.blob_hash, hash);
    assert_eq!(r1.received_at, 1_700_000_000_000);
    assert_eq!(r2.received_at, 1_700_000_001_000);
}

#[test]
fn create_email_returned_uid_matches_list_emails_in_mailbox() {
    // P7 UIDPLUS / APPENDUID regression: the (uid, uid_validity)
    // returned from `create_email` must equal the same row's per-
    // membership (seq.0, container.seq_validity) as projected by
    // `list_emails_in_mailbox`. If a future refactor splits the
    // returned UID read out of the same `with_set_tx`, this catches
    // the read-skew window.
    let (_d, mds, store) = open_mailstore();
    let _set = provision(&store);
    let inbox = store
        .create_mailbox(TEST_ACCOUNT, "Inbox", None, default_attrs())
        .unwrap();

    let (hash, _) = put_blob(&mds, b"appenduid regression");
    let env = sample_envelope();

    let (id, uid) = store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            hash,
            env,
            &[],
            Flags(0),
            Tags::new(),
            1_700_000_000_000,
        )
        .unwrap();
    assert_eq!(
        uid.container_id, inbox,
        "returned uid.container_id must be the dest mailbox"
    );
    assert!(uid.uid > 0, "APPENDUID must be > 0");

    let handles = store
        .list_emails_in_mailbox(
            TEST_ACCOUNT,
            inbox,
            ListOpts {
                limit: 16,
                offset: 0,
                sort_by: SortKey::Seq,
            },
        )
        .unwrap();
    let row = handles
        .iter()
        .find(|h| h.id == id)
        .expect("freshly-created item must be visible in list_emails_in_mailbox");
    assert_eq!(
        row.uid, uid.uid,
        "create_email returned uid must equal list_emails_in_mailbox uid"
    );
}

// -------------------- create_email_with_memberships --------------------
// Task 3.3 substrate (Option A hybrid). The multi-mailbox JMAP
// `Email/set create` / `Email/import` path: one with_set_tx, one
// aggregate Bus `mds.item.added`, IMAP-shaped (uid, uid_validity)
// returned per dest.

#[test]
fn create_email_with_memberships_happy_path_visible_via_get_email() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));
    let archive = mk_container(&mds, &set, "Archive", Some("\\Archive"));
    let drafts = mk_container(&mds, &set, "Drafts", Some("\\Drafts"));

    let payload = b"multi-mailbox delivery";
    let (hash, size) = put_blob(&mds, payload);
    let env = sample_envelope();
    let received_at: i64 = 1_730_000_000_000;

    let (id, uids) = store
        .create_email_with_memberships(
            TEST_ACCOUNT,
            &[inbox, archive, drafts],
            hash,
            env.clone(),
            &[],
            Flags(0b0001),
            Tags::new(),
            received_at,
        )
        .unwrap();

    // UID list mirrors input order, IMAP UIDPLUS-shaped per dest.
    assert_eq!(uids.len(), 3);
    assert_eq!(uids[0].container_id, inbox);
    assert_eq!(uids[1].container_id, archive);
    assert_eq!(uids[2].container_id, drafts);
    // First membership in each fresh container starts at seq=1.
    assert_eq!(uids[0].uid, 1);
    assert_eq!(uids[1].uid, 1);
    assert_eq!(uids[2].uid, 1);
    // Each container has its own seq_validity (set at row creation).
    // Different containers → typically different validities, but the
    // contract is they're observable, not that they differ.
    for u in &uids {
        assert!(u.uid_validity > 0, "seq_validity must be set, got 0");
    }

    // Visible end-to-end via the existing read path.
    let rec = store.get_email(TEST_ACCOUNT, id).unwrap();
    assert_eq!(rec.id, id);
    assert_eq!(rec.blob_hash, hash);
    assert_eq!(rec.size_bytes, size);
    assert_eq!(rec.received_at, received_at);
    assert_eq!(rec.envelope, env);
    assert_eq!(rec.keywords, Flags(0b0001));
    let mut got_mailboxes = rec.mailbox_ids.clone();
    got_mailboxes.sort_by_key(|c| c.0);
    let mut want_mailboxes = vec![inbox, archive, drafts];
    want_mailboxes.sort_by_key(|c| c.0);
    assert_eq!(got_mailboxes, want_mailboxes);
}

#[test]
fn create_email_with_memberships_single_membership_works() {
    // The JMAP `Email/set create` callsite is unconditional — it will
    // call this entry point with one mailbox when `mailboxIds` has one
    // entry, rather than branching to `create_email`. Pin that path.
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    let (hash, _) = put_blob(&mds, b"single-mailbox via multi entry");
    let env = sample_envelope();

    let (id, uids) = store
        .create_email_with_memberships(
            TEST_ACCOUNT,
            &[inbox],
            hash,
            env.clone(),
            &[],
            Flags(0),
            Tags::new(),
            1_730_000_000_000,
        )
        .unwrap();
    assert_eq!(uids.len(), 1);
    assert_eq!(uids[0].container_id, inbox);
    assert_eq!(uids[0].uid, 1);

    let rec = store.get_email(TEST_ACCOUNT, id).unwrap();
    assert_eq!(rec.mailbox_ids, vec![inbox]);
}

#[test]
fn create_email_with_memberships_rejects_empty() {
    let (_d, _mds, store) = open_mailstore();
    let _set = provision(&store);

    let phantom = cosmix_mds::blob::hash_bytes(b"never put");
    let err = store
        .create_email_with_memberships(
            TEST_ACCOUNT,
            &[],
            phantom,
            sample_envelope(),
            &[],
            Flags(0),
            Tags::new(),
            0,
        )
        .unwrap_err();
    let msg = format!("{err}");
    assert!(msg.starts_with("mailboxes must be non-empty"), "got: {msg}");
}

#[test]
fn create_email_with_memberships_rejects_duplicate_mailbox() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    let (hash, _) = put_blob(&mds, b"dup-mailbox guard");
    let err = store
        .create_email_with_memberships(
            TEST_ACCOUNT,
            &[inbox, inbox],
            hash,
            sample_envelope(),
            &[],
            Flags(0),
            Tags::new(),
            0,
        )
        .unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.starts_with("duplicate mailbox in mailboxes slice"),
        "got: {msg}"
    );

    // Prevalidation runs before tx; no item / blob_ref landed.
    assert_eq!(count_items(&mds, &set), 0);
    assert_eq!(count_blob_refs(&mds, &set), 0);
}

#[test]
fn create_email_with_memberships_unknown_mailbox_errors_with_mailbox_not_found() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    let (hash, _) = put_blob(&mds, b"partial-list reject");
    let bogus = ContainerId(uuid::Uuid::new_v4());

    let err = store
        .create_email_with_memberships(
            TEST_ACCOUNT,
            &[inbox, bogus],
            hash,
            sample_envelope(),
            &[],
            Flags(0),
            Tags::new(),
            0,
        )
        .unwrap_err();
    let msg = format!("{err}");
    assert!(msg.starts_with("mailbox not found"), "got: {msg}");

    // Prevalidation runs before any write — no partial item lands.
    assert_eq!(count_items(&mds, &set), 0);
    assert_eq!(count_blob_refs(&mds, &set), 0);
}

#[test]
fn create_email_with_memberships_unknown_blob_errors_with_blob_not_found() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));
    let archive = mk_container(&mds, &set, "Archive", Some("\\Archive"));

    let phantom = cosmix_mds::blob::hash_bytes(b"never put bytes");
    let err = store
        .create_email_with_memberships(
            TEST_ACCOUNT,
            &[inbox, archive],
            phantom,
            sample_envelope(),
            &[],
            Flags(0),
            Tags::new(),
            0,
        )
        .unwrap_err();
    let msg = format!("{err}");
    assert!(msg.starts_with("blob not found"), "got: {msg}");

    // Pre-write prevalidation passes (containers exist), but the
    // blob_size read fails before tx mutation. No partial state.
    assert_eq!(count_items(&mds, &set), 0);
    assert_eq!(count_blob_refs(&mds, &set), 0);
}

#[test]
fn create_email_with_memberships_unprovisioned_account_errors() {
    // No `provision` call: SetNotFound → "mailbox not found".
    let (_d, _mds, store) = open_mailstore();
    let phantom = cosmix_mds::blob::hash_bytes(b"into the void");
    let bogus = ContainerId(uuid::Uuid::new_v4());

    let err = store
        .create_email_with_memberships(
            TEST_ACCOUNT,
            &[bogus],
            phantom,
            sample_envelope(),
            &[],
            Flags(0),
            Tags::new(),
            0,
        )
        .unwrap_err();
    let msg = format!("{err}");
    assert!(msg.starts_with("mailbox not found"), "got: {msg}");
}

// -------------------- add_memberships --------------------
// Post-create membership additions for Task 3.4 `Email/set update`
// expanding `mailboxIds`.

#[test]
fn add_memberships_extends_mailbox_set() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));
    let archive = mk_container(&mds, &set, "Archive", Some("\\Archive"));
    let drafts = mk_container(&mds, &set, "Drafts", Some("\\Drafts"));

    let (hash, _) = put_blob(&mds, b"start in inbox");
    let (id, _uid) = store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            hash,
            sample_envelope(),
            &[],
            Flags(0b0001), // \Seen on creation
            Tags::new(),
            1_730_000_000_000,
        )
        .unwrap();

    let uids = store
        .add_memberships(TEST_ACCOUNT, id, &[archive, drafts])
        .unwrap();
    assert_eq!(uids.len(), 2);
    assert_eq!(uids[0].container_id, archive);
    assert_eq!(uids[0].uid, 1, "archive's first membership starts at seq=1");
    assert_eq!(uids[1].container_id, drafts);
    assert_eq!(uids[1].uid, 1);

    // After: 3 memberships. v1.1 §3 invariant — every membership
    // carries the same flags as the original (\Seen).
    let rec = store.get_email(TEST_ACCOUNT, id).unwrap();
    let mut got = rec.mailbox_ids.clone();
    got.sort_by_key(|c| c.0);
    let mut want = vec![inbox, archive, drafts];
    want.sort_by_key(|c| c.0);
    assert_eq!(got, want);
    assert_eq!(rec.keywords, Flags(0b0001), "v1.1 §3: \\Seen propagates");
}

#[test]
fn add_memberships_rejects_empty() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    let (hash, _) = put_blob(&mds, b"baseline");
    let (id, _uid) = store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            hash,
            sample_envelope(),
            &[],
            Flags(0),
            Tags::new(),
            0,
        )
        .unwrap();

    let err = store.add_memberships(TEST_ACCOUNT, id, &[]).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.starts_with("mailboxes must be non-empty"), "got: {msg}");
}

#[test]
fn add_memberships_rejects_duplicate_mailbox() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));
    let archive = mk_container(&mds, &set, "Archive", Some("\\Archive"));

    let (hash, _) = put_blob(&mds, b"dup-mailbox post-create");
    let (id, _uid) = store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            hash,
            sample_envelope(),
            &[],
            Flags(0),
            Tags::new(),
            0,
        )
        .unwrap();

    let err = store
        .add_memberships(TEST_ACCOUNT, id, &[archive, archive])
        .unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.starts_with("duplicate mailbox in mailboxes slice"),
        "got: {msg}"
    );

    // Email still has only its original membership.
    let rec = store.get_email(TEST_ACCOUNT, id).unwrap();
    assert_eq!(rec.mailbox_ids, vec![inbox]);
}

#[test]
fn add_memberships_rejects_already_member() {
    // The email is already in `inbox`; trying to add it to `inbox`
    // again must surface as a typed error so the JMAP layer can
    // distinguish "no-op, already there" from "added".
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));
    let archive = mk_container(&mds, &set, "Archive", Some("\\Archive"));

    let (hash, _) = put_blob(&mds, b"already-member guard");
    let (id, _uid) = store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            hash,
            sample_envelope(),
            &[],
            Flags(0),
            Tags::new(),
            0,
        )
        .unwrap();

    // Mix one new (archive) and one already-member (inbox); first
    // collision short-circuits, no partial fan-out should land.
    let err = store
        .add_memberships(TEST_ACCOUNT, id, &[archive, inbox])
        .unwrap_err();
    let msg = format!("{err}");
    assert!(msg.starts_with("item "), "got: {msg}");
    assert!(msg.contains("already in container"), "got: {msg}");

    // Tx rolled back: still only inbox membership.
    let rec = store.get_email(TEST_ACCOUNT, id).unwrap();
    assert_eq!(rec.mailbox_ids, vec![inbox]);
}

#[test]
fn add_memberships_unknown_email_errors_with_email_not_found() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    let phantom_id = ItemId(uuid::Uuid::new_v4());
    let err = store
        .add_memberships(TEST_ACCOUNT, phantom_id, &[inbox])
        .unwrap_err();
    let msg = format!("{err}");
    assert!(msg.starts_with("email not found"), "got: {msg}");
}

#[test]
fn add_memberships_unknown_mailbox_errors_with_mailbox_not_found() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    let (hash, _) = put_blob(&mds, b"unknown dest");
    let (id, _uid) = store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            hash,
            sample_envelope(),
            &[],
            Flags(0),
            Tags::new(),
            0,
        )
        .unwrap();

    let bogus = ContainerId(uuid::Uuid::new_v4());
    let err = store
        .add_memberships(TEST_ACCOUNT, id, &[bogus])
        .unwrap_err();
    let msg = format!("{err}");
    assert!(msg.starts_with("mailbox not found"), "got: {msg}");

    // Email still has only its original membership.
    let rec = store.get_email(TEST_ACCOUNT, id).unwrap();
    assert_eq!(rec.mailbox_ids, vec![inbox]);
}

#[test]
fn add_memberships_unprovisioned_account_errors_with_email_not_found() {
    // No provision: SetNotFound → "email not found".
    let (_d, _mds, store) = open_mailstore();
    let phantom_id = ItemId(uuid::Uuid::new_v4());
    let bogus_mb = ContainerId(uuid::Uuid::new_v4());
    let err = store
        .add_memberships(TEST_ACCOUNT, phantom_id, &[bogus_mb])
        .unwrap_err();
    let msg = format!("{err}");
    assert!(msg.starts_with("email not found"), "got: {msg}");
}

// ---- Task 1.8: set_keywords ----

#[test]
fn set_keywords_set_seen_is_visible_via_get_email() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    let (hash, _) = put_blob(&mds, b"unread message");
    let (id, _uid) = store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            hash,
            sample_envelope(),
            &[],
            Flags(0),
            Tags::new(),
            1_700_000_000_000,
        )
        .unwrap();

    // Initial: no keywords.
    let before = store.get_email(TEST_ACCOUNT, id).unwrap();
    assert_eq!(before.keywords, Flags(0));

    // \Seen = bit 0 in this codebase's Flags layout (see
    // `Flags::SEEN` in cosmix-mds/src/types.rs; matches existing
    // tests in this file that use Flags(0b0001) for the seen bit).
    store
        .set_keywords(TEST_ACCOUNT, id, Flags(0b0001), Tags::new())
        .unwrap();

    let after = store.get_email(TEST_ACCOUNT, id).unwrap();
    assert_eq!(after.keywords, Flags(0b0001));
}

#[test]
fn set_keywords_clear_flagged_is_visible_via_get_email() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    let (hash, _) = put_blob(&mds, b"flagged then cleared");
    // Start with both \Seen + \Flagged set so the "clear" is meaningful.
    let (id, _uid) = store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            hash,
            sample_envelope(),
            &[],
            Flags(0b0011),
            Tags::new(),
            1_700_000_000_000,
        )
        .unwrap();

    let before = store.get_email(TEST_ACCOUNT, id).unwrap();
    assert_eq!(before.keywords, Flags(0b0011));

    // Clear \Flagged (keep \Seen).
    store
        .set_keywords(TEST_ACCOUNT, id, Flags(0b0001), Tags::new())
        .unwrap();

    let after = store.get_email(TEST_ACCOUNT, id).unwrap();
    assert_eq!(after.keywords, Flags(0b0001));
}

#[test]
fn set_keywords_fans_out_across_all_memberships() {
    // JMAP `Email/set keywords` semantics — the keywords property is
    // the per-item union, so a write must touch every membership.
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));
    let archive = mk_container(&mds, &set, "Archive", None);

    let (hash, _) = put_blob(&mds, b"present in two mailboxes");
    let (id, _uid) = store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            hash,
            sample_envelope(),
            &[],
            Flags(0),
            Tags::new(),
            1_700_000_000_000,
        )
        .unwrap();

    // Add a second membership in Archive (alongside the original inbox).
    mds.copy_item(&set, &id, &archive, Flags(0)).unwrap();

    // Sanity: pre-state is no keywords on either membership.
    let memberships = mds.item_memberships(&set, &id).unwrap();
    assert_eq!(memberships.len(), 2);
    for (_cid, flags, _tags) in &memberships {
        assert_eq!(*flags, Flags(0));
    }

    store
        .set_keywords(TEST_ACCOUNT, id, Flags(0b0001), Tags::new())
        .unwrap();

    // Both membership rows now carry the new flags.
    let memberships = mds.item_memberships(&set, &id).unwrap();
    assert_eq!(memberships.len(), 2);
    for (_cid, flags, _tags) in &memberships {
        assert_eq!(*flags, Flags(0b0001));
    }
    // get_email returns the OR-union; the per-row loop above is the
    // load-bearing fan-out assertion. This `get_email` check is a
    // sanity belt-and-braces — it would still pass if only one
    // membership received the update (since `0 | 1 == 1`), so do
    // not read it as proof of fan-out by itself.
    let rec = store.get_email(TEST_ACCOUNT, id).unwrap();
    assert_eq!(rec.keywords, Flags(0b0001));
}

#[test]
fn set_keywords_unknown_id_errors_with_email_not_found() {
    let (_d, _mds, store) = open_mailstore();
    let _set = provision(&store);

    let bogus = ItemId(uuid::Uuid::new_v4());
    let err = store
        .set_keywords(TEST_ACCOUNT, bogus, Flags(0b0001), Tags::new())
        .unwrap_err();
    let msg = format!("{err}");
    assert!(msg.starts_with("email not found"), "got: {msg}");
}

#[test]
fn set_keywords_unprovisioned_account_errors_with_mailbox_not_found() {
    // No `provision` call: the account's set has never been bootstrapped.
    let (_d, _mds, store) = open_mailstore();
    let id = ItemId(uuid::Uuid::new_v4());
    let err = store
        .set_keywords(TEST_ACCOUNT, id, Flags(0b0001), Tags::new())
        .unwrap_err();
    let msg = format!("{err}");
    assert!(msg.starts_with("mailbox not found"), "got: {msg}");
}

// ---- Task 1.9: move_email ----

#[test]
fn move_email_between_mailboxes_reflected_in_list() {
    // Canonical case: email currently in Inbox, move to Archive.
    // After the move, list_emails_in_mailbox(inbox) is empty and
    // list_emails_in_mailbox(archive) contains the id. This pins the
    // observable post-condition that the migration plan calls out
    // ("reflected in list_emails_in_mailbox for both source and
    // destination").
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));
    let archive = mk_container(&mds, &set, "Archive", None);

    let (hash, _) = put_blob(&mds, b"will be archived");
    let (id, _uid) = store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            hash,
            sample_envelope(),
            &[],
            Flags(0),
            Tags::new(),
            1_700_000_000_000,
        )
        .unwrap();

    let before_inbox = store
        .list_emails_in_mailbox(TEST_ACCOUNT, inbox, ListOpts::default())
        .unwrap();
    assert_eq!(before_inbox.len(), 1);
    assert_eq!(before_inbox[0].id, id);

    store.move_email(TEST_ACCOUNT, id, archive).unwrap();

    let after_inbox = store
        .list_emails_in_mailbox(TEST_ACCOUNT, inbox, ListOpts::default())
        .unwrap();
    assert!(
        after_inbox.is_empty(),
        "inbox should be empty after move, got {after_inbox:?}"
    );

    let after_archive = store
        .list_emails_in_mailbox(TEST_ACCOUNT, archive, ListOpts::default())
        .unwrap();
    assert_eq!(after_archive.len(), 1);
    assert_eq!(after_archive[0].id, id);
}

#[test]
fn move_email_inherits_per_membership_flags() {
    // Pre-existing keywords on the src membership must survive the
    // move per v1.1 §3 ("all memberships agree" inheritance). The
    // `Flags(0)` arg on `tx.move_item` is the documented fallback,
    // not an override.
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));
    let archive = mk_container(&mds, &set, "Archive", None);

    let (hash, _) = put_blob(&mds, b"seen and archived");
    let (id, _uid) = store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            hash,
            sample_envelope(),
            &[],
            Flags(0b0011), // \Seen + \Flagged
            Tags::new(),
            1_700_000_000_000,
        )
        .unwrap();

    store.move_email(TEST_ACCOUNT, id, archive).unwrap();

    let memberships = mds.item_memberships(&set, &id).unwrap();
    assert_eq!(memberships.len(), 1);
    let (cid, flags, _tags) = &memberships[0];
    assert_eq!(*cid, archive);
    assert_eq!(*flags, Flags(0b0011), "flags must inherit from src");
}

#[test]
fn move_email_to_same_mailbox_is_noop() {
    // Idempotency: a JMAP client re-sending `mailboxIds: { inbox }`
    // when the email is already only in inbox must succeed without
    // touching the store.
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    let (hash, _) = put_blob(&mds, b"stays put");
    let (id, _uid) = store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            hash,
            sample_envelope(),
            &[],
            Flags(0b0001),
            Tags::new(),
            1_700_000_000_000,
        )
        .unwrap();

    store.move_email(TEST_ACCOUNT, id, inbox).unwrap();

    let memberships = mds.item_memberships(&set, &id).unwrap();
    assert_eq!(memberships.len(), 1);
    let (cid, flags, _tags) = &memberships[0];
    assert_eq!(*cid, inbox);
    assert_eq!(*flags, Flags(0b0001), "flags untouched on no-op");
}

#[test]
fn move_email_unknown_id_errors_with_email_not_found() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    let bogus = ItemId(uuid::Uuid::new_v4());
    let err = store.move_email(TEST_ACCOUNT, bogus, inbox).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.starts_with("email not found"), "got: {msg}");
}

#[test]
fn move_email_unknown_destination_errors_with_mailbox_not_found() {
    // Destination container does not exist in the account's set.
    // The src membership does, so the empty-fan-out branch does
    // not fire — the failure is in `tx.move_item` validating dest.
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    let (hash, _) = put_blob(&mds, b"move to nowhere");
    let (id, _uid) = store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            hash,
            sample_envelope(),
            &[],
            Flags(0),
            Tags::new(),
            1_700_000_000_000,
        )
        .unwrap();

    let bogus_dest = ContainerId(uuid::Uuid::new_v4());
    let err = store.move_email(TEST_ACCOUNT, id, bogus_dest).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.starts_with("mailbox not found"), "got: {msg}");
    // Pin that the message includes the offending UUID at all —
    // catches a regression that drops the id from the error
    // string. Note: in this test the dest *is* the missing
    // container, so a hypothetical regression that hard-codes
    // `to_mailbox.0` would coincidentally also produce this UUID;
    // the production impl extracts it from
    // `Error::ContainerNotFound(missing)` to be robust under DB-
    // corruption scenarios where src is missing instead, but those
    // scenarios are not constructible from public API and so are
    // not directly testable.
    assert!(
        msg.contains(&bogus_dest.0.to_string()),
        "error must echo the missing container UUID; got: {msg}"
    );
}

#[test]
fn move_email_multi_membership_errors_with_ambiguous_source() {
    // Email belongs to two mailboxes — JMAP "set mailboxIds" case,
    // not the simple move. `move_email` refuses; the JMAP layer
    // either uses `move_message` (with explicit src) or waits for
    // the future `set_mailboxes` API.
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));
    let archive = mk_container(&mds, &set, "Archive", None);
    let trash = mk_container(&mds, &set, "Trash", Some("\\Trash"));

    let (hash, _) = put_blob(&mds, b"multi-homed");
    let (id, _uid) = store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            hash,
            sample_envelope(),
            &[],
            Flags(0),
            Tags::new(),
            1_700_000_000_000,
        )
        .unwrap();
    mds.copy_item(&set, &id, &archive, Flags(0)).unwrap();

    let err = store.move_email(TEST_ACCOUNT, id, trash).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.starts_with("ambiguous source"), "got: {msg}");

    // Pre-state preserved — neither membership was touched.
    let memberships = mds.item_memberships(&set, &id).unwrap();
    assert_eq!(memberships.len(), 2);
    let cids: Vec<ContainerId> = memberships.iter().map(|(c, _, _)| *c).collect();
    assert!(cids.contains(&inbox));
    assert!(cids.contains(&archive));
    assert!(!cids.contains(&trash));
}

#[test]
fn move_email_unprovisioned_account_errors_with_mailbox_not_found() {
    let (_d, _mds, store) = open_mailstore();
    let id = ItemId(uuid::Uuid::new_v4());
    let dest = ContainerId(uuid::Uuid::new_v4());
    let err = store.move_email(TEST_ACCOUNT, id, dest).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.starts_with("mailbox not found"), "got: {msg}");
}

// ---- Task 1.10: delete_email ----

fn envelope_row_exists(mds: &SqliteCasMds, set: &SetId, id: &ItemId) -> bool {
    let item_id_s = id.0.to_string();
    let mut found = false;
    mds.with_set_tx(set, |tx| {
        let n: i64 = tx
            .tx()
            .query_row(
                "SELECT COUNT(*) FROM mail_envelopes WHERE item_id = ?1",
                params![item_id_s],
                |r| r.get(0),
            )
            .unwrap();
        found = n > 0;
        Ok(())
    })
    .unwrap();
    found
}

#[test]
fn delete_email_not_visible_via_get_email() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    let (hash, _) = put_blob(&mds, b"to be deleted");
    let (id, _uid) = store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            hash,
            sample_envelope(),
            &[],
            Flags(0),
            Tags::new(),
            1_700_000_000_000,
        )
        .unwrap();

    // Pre-state: gettable + envelope row present.
    assert!(store.get_email(TEST_ACCOUNT, id).is_ok());
    assert!(envelope_row_exists(&mds, &set, &id));

    store.delete_email(TEST_ACCOUNT, id).unwrap();

    let err = store.get_email(TEST_ACCOUNT, id).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.starts_with("email not found"), "got: {msg}");
}

#[test]
fn delete_email_drops_item_row_and_envelope_via_cascade() {
    // FK `mail_envelopes.item_id ON DELETE CASCADE` removes the
    // sidecar in the same SQL statement as the `item` row delete.
    // Pin both halves: item row gone (fetch_item_meta →
    // ItemNotFound) and envelope row gone (raw COUNT == 0).
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    let (hash, _) = put_blob(&mds, b"cascade target");
    let (id, _uid) = store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            hash,
            sample_envelope(),
            &[],
            Flags(0),
            Tags::new(),
            1_700_000_000_000,
        )
        .unwrap();

    store.delete_email(TEST_ACCOUNT, id).unwrap();

    let item_err = mds.fetch_item_meta(&set, &id).unwrap_err();
    assert!(
        matches!(item_err, cosmix_mds::Error::ItemNotFound(_)),
        "item row should be gone; got: {item_err:?}"
    );
    assert!(
        !envelope_row_exists(&mds, &set, &id),
        "mail_envelopes row should be cascaded"
    );
}

#[test]
fn delete_email_clears_mailbox_listing() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    let (hash, _) = put_blob(&mds, b"removed from inbox");
    let (id, _uid) = store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            hash,
            sample_envelope(),
            &[],
            Flags(0),
            Tags::new(),
            1_700_000_000_000,
        )
        .unwrap();

    let before = store
        .list_emails_in_mailbox(TEST_ACCOUNT, inbox, ListOpts::default())
        .unwrap();
    assert_eq!(before.len(), 1);

    store.delete_email(TEST_ACCOUNT, id).unwrap();

    let after = store
        .list_emails_in_mailbox(TEST_ACCOUNT, inbox, ListOpts::default())
        .unwrap();
    assert!(
        after.is_empty(),
        "inbox should be empty after delete; got {after:?}"
    );
}

#[test]
fn delete_email_removes_all_memberships_for_multi_homed() {
    // Multi-membership delete: every membership row is gone
    // post-call. This pins the *outcome* — zero memberships and
    // the item row removed — not the *mechanism* (whether each
    // remove_membership was called in turn vs a single bulk
    // delete). The orphan cascade (item row + blob_ref drop) is
    // covered by `delete_email_drops_item_row_and_envelope_via_cascade`.
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));
    let archive = mk_container(&mds, &set, "Archive", None);

    let (hash, _) = put_blob(&mds, b"in two places");
    let (id, _uid) = store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            hash,
            sample_envelope(),
            &[],
            Flags(0),
            Tags::new(),
            1_700_000_000_000,
        )
        .unwrap();
    mds.copy_item(&set, &id, &archive, Flags(0)).unwrap();

    let pre = mds.item_memberships(&set, &id).unwrap();
    assert_eq!(pre.len(), 2);

    store.delete_email(TEST_ACCOUNT, id).unwrap();

    let post = mds.item_memberships(&set, &id).unwrap();
    assert!(post.is_empty(), "all memberships removed; got {post:?}");
    let item_err = mds.fetch_item_meta(&set, &id).unwrap_err();
    assert!(
        matches!(item_err, cosmix_mds::Error::ItemNotFound(_)),
        "item row should be gone after last membership removal"
    );
}

#[test]
fn delete_email_unknown_id_errors_with_email_not_found() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let _inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    let bogus = ItemId(uuid::Uuid::new_v4());
    let err = store.delete_email(TEST_ACCOUNT, bogus).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.starts_with("email not found"), "got: {msg}");
}

#[test]
fn delete_email_unprovisioned_account_errors_with_mailbox_not_found() {
    let (_d, _mds, store) = open_mailstore();
    let id = ItemId(uuid::Uuid::new_v4());
    let err = store.delete_email(TEST_ACCOUNT, id).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.starts_with("mailbox not found"), "got: {msg}");
}

#[test]
fn delete_email_isolates_from_sibling_items() {
    // Cross-item isolation: deleting one email leaves *other*
    // emails in the same set untouched. A regression that walked
    // the whole set instead of the target item's memberships
    // would be caught here.
    //
    // Note: multi-membership rollback atomicity (mid-iteration
    // failure leaves no membership removed) is correct by
    // construction — `with_set_tx` rolls the SQLite tx back on
    // any `Err`. A fault-injection test would require invasive
    // scaffolding (mid-tx error hook); deferred to Phase 1
    // cleanup.
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    let (hash_a, _) = put_blob(&mds, b"keep me");
    let (hash_b, _) = put_blob(&mds, b"delete me");
    let (keep, _keep_uid) = store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            hash_a,
            sample_envelope(),
            &[],
            Flags(0),
            Tags::new(),
            1_700_000_000_000,
        )
        .unwrap();
    let (drop, _drop_uid) = store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            hash_b,
            sample_envelope(),
            &[],
            Flags(0),
            Tags::new(),
            1_700_000_001_000,
        )
        .unwrap();

    store.delete_email(TEST_ACCOUNT, drop).unwrap();

    assert!(store.get_email(TEST_ACCOUNT, keep).is_ok());
    assert!(store.get_email(TEST_ACCOUNT, drop).is_err());
}

// -------------------- mail_search FTS projection (Phase 8e gate 2) --------------------

/// Probe helper — open a per-set sqlite connection, run a MATCH, and
/// return the count of rows for the given item_id. Uses a probe tx
/// (always returns Err) so the read does not allocate set_change
/// rows. `mail_search` is a contentless FTS5 virtual table living in
/// the per-set DB next to the standard sidecars.
fn count_mail_search_match(mds: &SqliteCasMds, set: &SetId, item: &ItemId, needle: &str) -> i64 {
    let mut out = 0i64;
    let _: cosmix_mds::Result<()> = mds.with_set_tx(set, |tx| {
        out = tx
            .tx()
            .query_row(
                "SELECT COUNT(*) FROM mail_search \
                 WHERE mail_search MATCH ?1 AND item_id = ?2",
                params![needle, item.0.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        Err(cosmix_mds::Error::Other("probe".into()))
    });
    out
}

fn count_mail_search_rows_for_item(mds: &SqliteCasMds, set: &SetId, item: &ItemId) -> i64 {
    let mut out = 0i64;
    let _: cosmix_mds::Result<()> = mds.with_set_tx(set, |tx| {
        out = tx
            .tx()
            .query_row(
                "SELECT COUNT(*) FROM mail_search WHERE item_id = ?1",
                params![item.0.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        Err(cosmix_mds::Error::Other("probe".into()))
    });
    out
}

#[test]
fn create_email_writes_fts_row_matched_by_subject() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    // Use a real RFC 5322 message so body_text indexes too.
    let raw = b"From: alice@example.com\r\nTo: bob@example.com\r\nSubject: lunch?\r\nContent-Type: text/plain\r\n\r\nshall we meet at the cafe today\r\n";
    let (hash, _) = put_blob(&mds, raw);
    let env = sample_envelope();
    let (id, _uid) = store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            hash,
            env,
            &[],
            Flags(0),
            Tags::new(),
            1_700_000_000_000,
        )
        .unwrap();

    assert_eq!(count_mail_search_rows_for_item(&mds, &set, &id), 1);
    assert_eq!(count_mail_search_match(&mds, &set, &id, "lunch"), 1);
}

#[test]
fn create_email_indexes_body_text() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    let raw = b"From: alice@example.com\r\nSubject: hi\r\nContent-Type: text/plain\r\n\r\ndistinctive token: pelican\r\n";
    let (hash, _) = put_blob(&mds, raw);
    let (id, _) = store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            hash,
            sample_envelope(),
            &[],
            Flags(0),
            Tags::new(),
            1_700_000_000_000,
        )
        .unwrap();

    assert_eq!(count_mail_search_match(&mds, &set, &id, "pelican"), 1);
    assert_eq!(
        count_mail_search_match(&mds, &set, &id, "nonexistent_token"),
        0
    );
}

#[test]
fn create_email_indexes_normalized_addrs() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    let raw = b"From: alice@example.com\r\nTo: bob@example.com\r\nSubject: hi\r\n\r\n";
    let (hash, _) = put_blob(&mds, raw);
    let env = EmailEnvelope {
        from: "alice@example.com".into(),
        to: vec!["carol@distinct-domain-xyz.test".into()],
        cc: vec![],
        bcc: vec![],
        reply_to: vec![],
        subject: "hi".into(),
        date: 1_700_000_000,
        message_id: None,
    };
    let (id, _) = store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            hash,
            env,
            &[],
            Flags(0),
            Tags::new(),
            1_700_000_000_000,
        )
        .unwrap();

    // FTS tokenises on `.` and `@`, so a phrase MATCH on the unique
    // domain identifies this row even without the localpart.
    assert_eq!(
        count_mail_search_match(&mds, &set, &id, "\"distinct domain xyz\""),
        1
    );
}

#[test]
fn delete_email_removes_fts_row() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    let raw = b"From: alice@example.com\r\nSubject: lunch?\r\nContent-Type: text/plain\r\n\r\ndelete me please\r\n";
    let (hash, _) = put_blob(&mds, raw);
    let (id, _) = store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            hash,
            sample_envelope(),
            &[],
            Flags(0),
            Tags::new(),
            1_700_000_000_000,
        )
        .unwrap();
    assert_eq!(count_mail_search_rows_for_item(&mds, &set, &id), 1);

    store.delete_email(TEST_ACCOUNT, id).unwrap();
    assert_eq!(count_mail_search_rows_for_item(&mds, &set, &id), 0);
}

#[test]
fn create_email_with_memberships_writes_single_fts_row() {
    // Phase 8e invariant: the FTS projection is item-scoped, not
    // membership-scoped. Multi-mailbox delivery (sieve fileinto +
    // Inbox simultaneously) must NOT duplicate the row.
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));
    let archive = mk_container(&mds, &set, "Archive", Some("\\Archive"));

    let raw = b"From: alice@example.com\r\nSubject: multi\r\nContent-Type: text/plain\r\n\r\nshared body bytes\r\n";
    let (hash, _) = put_blob(&mds, raw);
    let (id, _) = store
        .create_email_with_memberships(
            TEST_ACCOUNT,
            &[inbox, archive],
            hash,
            sample_envelope(),
            &[],
            Flags(0),
            Tags::new(),
            1_700_000_000_000,
        )
        .unwrap();

    assert_eq!(count_mail_search_rows_for_item(&mds, &set, &id), 1);
    assert_eq!(count_mail_search_match(&mds, &set, &id, "shared"), 1);
}

#[test]
fn delete_email_removes_fts_row_for_multi_homed_item() {
    // `delete_email` iterates every membership and then DELETEs the
    // FTS row inside the same with_set_tx. Verify the FTS reap fires
    // exactly once for a multi-homed item.
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));
    let archive = mk_container(&mds, &set, "Archive", Some("\\Archive"));

    let raw =
        b"From: alice@example.com\r\nSubject: doomed\r\nContent-Type: text/plain\r\n\r\nbody\r\n";
    let (hash, _) = put_blob(&mds, raw);
    let (id, _) = store
        .create_email_with_memberships(
            TEST_ACCOUNT,
            &[inbox, archive],
            hash,
            sample_envelope(),
            &[],
            Flags(0),
            Tags::new(),
            1_700_000_000_000,
        )
        .unwrap();
    assert_eq!(count_mail_search_rows_for_item(&mds, &set, &id), 1);

    store.delete_email(TEST_ACCOUNT, id).unwrap();
    assert_eq!(count_mail_search_rows_for_item(&mds, &set, &id), 0);
}

/// Phase 8e gate 2: destroy_mailbox(Delete) reaps the FTS row for
/// every item whose last membership it drops. The item row is gone
/// (cascade from `remove_membership`), so the matching FTS row must
/// not be left dangling. Reach this path via create_email so the
/// initial FTS row exists.
#[test]
fn destroy_mailbox_delete_reaps_fts_rows() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));
    let bin = store
        .create_mailbox(TEST_ACCOUNT, "Bin", None, default_attrs())
        .unwrap();

    let raw =
        b"From: alice@example.com\r\nSubject: doomed\r\nContent-Type: text/plain\r\n\r\nbody\r\n";
    let (hash, _) = put_blob(&mds, raw);
    // Single-membership item in `bin` — destroy_mailbox(Delete) on
    // `bin` will be the last membership.
    let (id, _) = store
        .create_email(
            TEST_ACCOUNT,
            bin,
            hash,
            sample_envelope(),
            &[],
            Flags(0),
            Tags::new(),
            1_700_000_000_000,
        )
        .unwrap();
    assert_eq!(count_mail_search_rows_for_item(&mds, &set, &id), 1);

    store
        .destroy_mailbox(TEST_ACCOUNT, bin, DestroyPolicy::Delete)
        .unwrap();

    // Item row is gone — verify the FTS row was reaped in the same tx.
    assert!(mds.fetch_item_meta(&set, &id).is_err());
    assert_eq!(
        count_mail_search_rows_for_item(&mds, &set, &id),
        0,
        "destroy_mailbox(Delete) must reap mail_search rows for items whose last membership was dropped"
    );
    // Inbox stays so the open_mailstore happy-path invariants survive.
    let _ = inbox;
}

/// Inverse: destroy_mailbox(Delete) must NOT reap the FTS row when
/// the item survives via another membership. The `NOT EXISTS` guard
/// in `reap_mail_search_if_item_gone` is the load-bearing predicate.
#[test]
fn destroy_mailbox_delete_preserves_fts_for_items_in_other_mailboxes() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));
    let bin = store
        .create_mailbox(TEST_ACCOUNT, "Bin", None, default_attrs())
        .unwrap();

    let raw = b"From: alice@example.com\r\nSubject: shared\r\nContent-Type: text/plain\r\n\r\nshared body\r\n";
    let (hash, _) = put_blob(&mds, raw);
    let (id, _) = store
        .create_email_with_memberships(
            TEST_ACCOUNT,
            &[inbox, bin],
            hash,
            sample_envelope(),
            &[],
            Flags(0),
            Tags::new(),
            1_700_000_000_000,
        )
        .unwrap();
    assert_eq!(count_mail_search_rows_for_item(&mds, &set, &id), 1);

    store
        .destroy_mailbox(TEST_ACCOUNT, bin, DestroyPolicy::Delete)
        .unwrap();

    // Item survives in inbox; FTS row must too.
    mds.fetch_item_meta(&set, &id).unwrap();
    assert_eq!(
        count_mail_search_rows_for_item(&mds, &set, &id),
        1,
        "destroy_mailbox(Delete) must NOT reap mail_search for items that survive in other mailboxes"
    );
}

/// Phase 8e gate 2: `expunge_memberships` reaps FTS for items whose
/// last membership was dropped (no other mailbox holds them).
#[test]
fn expunge_memberships_reaps_fts_when_last_membership() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    let raw =
        b"From: alice@example.com\r\nSubject: expunge\r\nContent-Type: text/plain\r\n\r\nbody\r\n";
    let (hash, _) = put_blob(&mds, raw);
    let (id, _) = store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            hash,
            sample_envelope(),
            &[],
            Flags(0),
            Tags::new(),
            1_700_000_000_000,
        )
        .unwrap();
    assert_eq!(count_mail_search_rows_for_item(&mds, &set, &id), 1);

    let n = store
        .expunge_memberships(TEST_ACCOUNT, inbox, &[id])
        .unwrap();
    assert_eq!(n, 1);
    assert!(mds.fetch_item_meta(&set, &id).is_err());
    assert_eq!(
        count_mail_search_rows_for_item(&mds, &set, &id),
        0,
        "expunge_memberships must reap mail_search when the item row is dropped"
    );
}

/// Inverse: expunge_memberships from one mailbox must NOT reap FTS
/// when the item survives in another mailbox.
#[test]
fn expunge_memberships_preserves_fts_when_item_survives() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));
    let archive = mk_container(&mds, &set, "Archive", Some("\\Archive"));

    let raw =
        b"From: alice@example.com\r\nSubject: stays\r\nContent-Type: text/plain\r\n\r\nbody\r\n";
    let (hash, _) = put_blob(&mds, raw);
    let (id, _) = store
        .create_email_with_memberships(
            TEST_ACCOUNT,
            &[inbox, archive],
            hash,
            sample_envelope(),
            &[],
            Flags(0),
            Tags::new(),
            1_700_000_000_000,
        )
        .unwrap();
    assert_eq!(count_mail_search_rows_for_item(&mds, &set, &id), 1);

    let n = store
        .expunge_memberships(TEST_ACCOUNT, inbox, &[id])
        .unwrap();
    assert_eq!(n, 1);
    // Item survives in archive; FTS row must too.
    mds.fetch_item_meta(&set, &id).unwrap();
    assert_eq!(
        count_mail_search_rows_for_item(&mds, &set, &id),
        1,
        "expunge_memberships must NOT reap mail_search when item survives in another mailbox"
    );
}

#[test]
fn fts_match_isolated_between_items() {
    // Two distinct items in the same set must each get their own FTS
    // row; a MATCH on item A's unique token must NOT find item B's
    // row. Catches a regression that wrote the projection without an
    // item_id bind (rowid-implicit) or that reused a stale projection
    // across calls.
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    let raw_a = b"From: a@x\r\nSubject: hi\r\nContent-Type: text/plain\r\n\r\nuniquetokena\r\n";
    let raw_b = b"From: b@x\r\nSubject: hi\r\nContent-Type: text/plain\r\n\r\nuniquetokenb\r\n";
    let (ha, _) = put_blob(&mds, raw_a);
    let (hb, _) = put_blob(&mds, raw_b);
    let (ida, _) = store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            ha,
            sample_envelope(),
            &[],
            Flags(0),
            Tags::new(),
            1_700_000_000_000,
        )
        .unwrap();
    let (idb, _) = store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            hb,
            sample_envelope(),
            &[],
            Flags(0),
            Tags::new(),
            1_700_000_001_000,
        )
        .unwrap();

    assert_eq!(count_mail_search_match(&mds, &set, &ida, "uniquetokena"), 1);
    assert_eq!(count_mail_search_match(&mds, &set, &ida, "uniquetokenb"), 0);
    assert_eq!(count_mail_search_match(&mds, &set, &idb, "uniquetokenb"), 1);
    assert_eq!(count_mail_search_match(&mds, &set, &idb, "uniquetokena"), 0);
}

// ---- Task 1.11 — email_changes ----

#[test]
fn email_changes_no_events_returns_empty_at_since_state() {
    let (_d, _mds, store) = open_mailstore();
    let _set = provision(&store);

    let cs = store.email_changes(TEST_ACCOUNT, 0).unwrap();
    assert_eq!(cs.created.len(), 0);
    assert_eq!(cs.updated.len(), 0);
    assert_eq!(cs.destroyed.len(), 0);
    // No set_change rows allocated yet → new_state stays at since.
    assert_eq!(cs.new_state, 0);
}

#[test]
fn email_changes_create_then_query_from_zero_classifies_as_created() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    let (hash, _) = put_blob(&mds, b"new mail");
    let (id, _uid) = store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            hash,
            sample_envelope(),
            &[],
            Flags(0),
            Tags::new(),
            1_700_000_000_000,
        )
        .unwrap();

    let cs = store.email_changes(TEST_ACCOUNT, 0).unwrap();
    assert_eq!(cs.created, vec![id]);
    assert!(cs.updated.is_empty());
    assert!(cs.destroyed.is_empty());
    assert!(cs.new_state > 0, "new_state should advance past 0");
}

#[test]
fn email_changes_update_after_since_state_classifies_as_updated() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    let (hash, _) = put_blob(&mds, b"existing mail");
    let (id, _uid) = store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            hash,
            sample_envelope(),
            &[],
            Flags(0),
            Tags::new(),
            1_700_000_000_000,
        )
        .unwrap();

    // Capture state s0 *after* creation so the create itself is
    // outside the window.
    let s0 = store.email_changes(TEST_ACCOUNT, 0).unwrap().new_state;
    assert!(s0 > 0);

    // Update keywords — generates MetadataChanged events.
    store
        .set_keywords(TEST_ACCOUNT, id, Flags(0b0001), Tags::new())
        .unwrap();

    let cs = store.email_changes(TEST_ACCOUNT, s0).unwrap();
    assert!(cs.created.is_empty());
    assert_eq!(cs.updated, vec![id]);
    assert!(cs.destroyed.is_empty());
    assert!(cs.new_state > s0);
}

#[test]
fn email_changes_destroy_after_since_state_classifies_as_destroyed() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    let (hash, _) = put_blob(&mds, b"to be deleted");
    let (id, _uid) = store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            hash,
            sample_envelope(),
            &[],
            Flags(0),
            Tags::new(),
            1_700_000_000_000,
        )
        .unwrap();

    let s0 = store.email_changes(TEST_ACCOUNT, 0).unwrap().new_state;

    store.delete_email(TEST_ACCOUNT, id).unwrap();

    let cs = store.email_changes(TEST_ACCOUNT, s0).unwrap();
    assert!(cs.created.is_empty());
    assert!(cs.updated.is_empty());
    assert_eq!(cs.destroyed, vec![id]);
    assert!(cs.new_state > s0);
}

#[test]
fn email_changes_create_and_destroy_inside_window_omits_item() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    let s0 = store.email_changes(TEST_ACCOUNT, 0).unwrap().new_state;
    assert_eq!(s0, 0);

    let (hash, _) = put_blob(&mds, b"transient");
    let (id, _uid) = store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            hash,
            sample_envelope(),
            &[],
            Flags(0),
            Tags::new(),
            1_700_000_000_000,
        )
        .unwrap();
    store.delete_email(TEST_ACCOUNT, id).unwrap();

    let cs = store.email_changes(TEST_ACCOUNT, s0).unwrap();
    // RFC 8621 §4.1.5: an item created and destroyed in the same
    // window was never observed by the client and must not appear
    // in any of the three arrays.
    assert!(cs.created.is_empty(), "created: {:?}", cs.created);
    assert!(cs.updated.is_empty(), "updated: {:?}", cs.updated);
    assert!(cs.destroyed.is_empty(), "destroyed: {:?}", cs.destroyed);
    assert!(cs.new_state > s0);
}

#[test]
fn email_changes_move_after_since_state_classifies_as_updated() {
    // A move is two set_change rows (Removed from src + Added to
    // dest) on the same item. existed_at_since=true (first event
    // is Removed), exists_now=true (still has dest membership) →
    // updated. This is the JMAP-correct shape per RFC 8621 §4.1.5.
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));
    let archive = mk_container(&mds, &set, "Archive", None);

    let (hash, _) = put_blob(&mds, b"to be moved");
    let (id, _uid) = store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            hash,
            sample_envelope(),
            &[],
            Flags(0),
            Tags::new(),
            1_700_000_000_000,
        )
        .unwrap();

    let s0 = store.email_changes(TEST_ACCOUNT, 0).unwrap().new_state;

    store.move_email(TEST_ACCOUNT, id, archive).unwrap();

    let cs = store.email_changes(TEST_ACCOUNT, s0).unwrap();
    assert!(cs.created.is_empty(), "created: {:?}", cs.created);
    assert_eq!(cs.updated, vec![id]);
    assert!(cs.destroyed.is_empty());
    assert!(cs.new_state > s0);
}

#[test]
fn email_changes_unprovisioned_account_with_zero_state_returns_empty_delta() {
    // Mirrors `email_state`'s zero-token baseline for unprovisioned
    // accounts: a client calling Email/changes with `sinceState="0"`
    // against an empty account must observe an empty delta, not
    // `cannotCalculateChanges`. Email/get + Email/query already
    // succeed against unprovisioned sets via `email_state == 0`,
    // and the changes surface must agree.
    let (_d, _mds, store) = open_mailstore();
    let cs = store.email_changes(TEST_ACCOUNT, 0).unwrap();
    assert_eq!(cs.new_state, 0);
    assert!(cs.created.is_empty());
    assert!(cs.updated.is_empty());
    assert!(cs.destroyed.is_empty());
}

#[test]
fn email_changes_rejects_since_state_above_i64_max() {
    // changes_since_set casts the token `as i64` at the SQL
    // boundary; a u64 above i64::MAX would silently wrap and
    // return the full table. Surface a clear error instead.
    let (_d, _mds, store) = open_mailstore();
    let _set = provision(&store);

    let bad = (i64::MAX as u64).saturating_add(1);
    let err = store.email_changes(TEST_ACCOUNT, bad).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.starts_with("since_state out of range"), "got: {msg}");
}

#[test]
fn email_changes_future_since_state_errors() {
    // RFC 8620 §5.2: a `sinceState` token above the per-set
    // head must surface as `cannotCalculateChanges`. The
    // mailstore primitive returns the `"future since_state"`
    // prefix; the JMAP handler then maps it. Without the guard,
    // a future token returned an empty page and `unwrap_or`
    // echoed it back — subsequent real changes whose seq is
    // `<= since_state` would be permanently skipped.
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));
    let (hash, _) = put_blob(&mds, b"hi");
    let _ = store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            hash,
            sample_envelope(),
            &[],
            Flags(0),
            Tags::new(),
            1_700_000_000_000,
        )
        .unwrap();
    let head = store.email_changes(TEST_ACCOUNT, 0).unwrap().new_state;
    assert!(head > 0);

    // since_state == head is valid (no changes since last sync).
    let at_head = store.email_changes(TEST_ACCOUNT, head).unwrap();
    assert!(at_head.created.is_empty());
    assert!(at_head.updated.is_empty());
    assert!(at_head.destroyed.is_empty());
    assert_eq!(at_head.new_state, head);

    // since_state == head + 1 is the "future" case.
    let err = store
        .email_changes(TEST_ACCOUNT, head.saturating_add(1))
        .unwrap_err()
        .to_string();
    assert!(
        err.starts_with("future since_state"),
        "expected `future since_state` prefix, got: {err}"
    );
}

#[test]
fn email_changes_future_since_state_on_unprovisioned_set_routes_to_cannot_calculate() {
    // Unprovisioned sets have an implicit head of 0 (matching
    // `email_state`). Any non-zero `since_state` against them is
    // therefore "above the head" and must surface with the
    // `"future since_state"` prefix so the JMAP handler maps it to
    // `cannotCalculateChanges` — not as a "mailbox not found"
    // surprise the client never asked about. `since_state == 0` is
    // the empty-delta path covered by the sibling test above.
    let (_d, _mds, store) = open_mailstore();
    let err = store.email_changes(TEST_ACCOUNT, 12345).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.starts_with("future since_state"),
        "expected `future since_state` prefix, got: {msg}"
    );
}

#[test]
fn email_changes_filters_out_staging_container_uploads() {
    // `create_blob_ref` lands a staging item via
    // `add_staging_item_in_tx`, which writes an `Added` row to
    // `set_change`. Without the per-account staging filter, that
    // would leak as a phantom `created` ItemId in `Email/changes`
    // — JMAP clients must not see staging items as emails.
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    // Land an upload via create_blob_ref (allocates `Added` rows
    // in set_change for the staging container).
    let (hash, size) = put_blob(&mds, b"upload payload");
    let _alias = store
        .create_blob_ref(TEST_ACCOUNT, hash, size, 9_999_999_999)
        .unwrap();

    // Also create one real email so we know the change stream is
    // active and email_changes returns *something* — proving the
    // filter excludes staging events specifically rather than
    // returning empty by accident.
    let (hash2, _) = put_blob(&mds, b"real email");
    let (real, _real_uid) = store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            hash2,
            sample_envelope(),
            &[],
            Flags(0),
            Tags::new(),
            1_700_000_000_000,
        )
        .unwrap();

    let cs = store.email_changes(TEST_ACCOUNT, 0).unwrap();
    assert_eq!(cs.created, vec![real]);
    assert!(cs.updated.is_empty());
    assert!(cs.destroyed.is_empty());
    // new_state advanced past the staging events too — clients
    // track the seq cursor regardless of which events landed.
    assert!(cs.new_state > 0);
}

// ---- email_state (Task 18) ----

#[test]
fn email_state_unprovisioned_account_returns_zero() {
    let (_d, _mds, store) = open_mailstore();
    // No `provision(&store)` — account set has never been created.
    let head = store.email_state(TEST_ACCOUNT).unwrap();
    assert_eq!(head, 0);
}

#[test]
fn email_state_provisioned_no_events_returns_zero() {
    let (_d, _mds, store) = open_mailstore();
    let _set = provision(&store);
    let head = store.email_state(TEST_ACCOUNT).unwrap();
    assert_eq!(head, 0);
}

#[test]
fn email_state_advances_with_create_and_matches_email_changes_new_state() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    let (hash, _) = put_blob(&mds, b"hello");
    let (_id, _uid) = store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            hash,
            sample_envelope(),
            &[],
            Flags(0),
            Tags::new(),
            1_700_000_000_000,
        )
        .unwrap();

    let head = store.email_state(TEST_ACCOUNT).unwrap();
    assert!(head > 0, "head should advance past zero after create");

    // Cheap accessor must agree with the full-walk new_state.
    let cs = store.email_changes(TEST_ACCOUNT, 0).unwrap();
    assert_eq!(head, cs.new_state);
}

// ---- mailbox_state / mailbox_changes (MCS Phase 2) -----------------------

fn parse_paired(s: &str) -> (u64, u64) {
    let (l, r) = s.split_once(':').unwrap();
    (l.parse().unwrap(), r.parse().unwrap())
}

#[test]
fn mailbox_state_unprovisioned_returns_zero_paired() {
    let (_d, _mds, store) = open_mailstore();
    assert_eq!(store.mailbox_state(TEST_ACCOUNT).unwrap(), "0:0");
}

#[test]
fn mailbox_state_provisioned_no_events_returns_zero_paired() {
    let (_d, _mds, store) = open_mailstore();
    let _set = provision(&store);
    // provision() bootstraps the set but does not itself emit a
    // lifecycle row; no container has been created yet.
    assert_eq!(store.mailbox_state(TEST_ACCOUNT).unwrap(), "0:0");
}

#[test]
fn mailbox_state_advances_lifecycle_half_after_create_mailbox() {
    let (_d, mds, store) = open_mailstore();
    let _set = provision(&store);
    let _id = store
        .create_mailbox(
            TEST_ACCOUNT,
            "Inbox",
            None,
            ContainerAttrs {
                special_use: Some("\\Inbox".into()),
                subscribed: false,
                extra: serde_json::json!({}),
            },
        )
        .unwrap();
    let (l, s) = parse_paired(&store.mailbox_state(TEST_ACCOUNT).unwrap());
    assert!(l > 0, "lifecycle half should advance after create_mailbox");
    // No item membership churn happened so set_change half is unchanged.
    assert_eq!(s, 0);
    let _ = mds; // keep the variable alive
}

#[test]
fn mailbox_changes_invalid_since_state_syntax() {
    let (_d, _mds, store) = open_mailstore();
    let _set = provision(&store);
    for bad in [
        "", "abc", "1", ":", ":5", "5:", "1:2:3", "+1:0", "1:-1", "x:y",
        // Non-canonical leading zeros must not alias `"1:2"` —
        // a future encoding (e.g. base32) lands cleanly if the
        // grammar is strictly canonical decimal today.
        "01:2", "1:02", "00:0", " 1:2", "1: 2",
    ] {
        let err = store
            .mailbox_changes(TEST_ACCOUNT, bad, 100)
            .expect_err(&format!("token {bad:?} should be rejected"));
        let msg = err.to_string();
        assert!(
            msg.starts_with("invalid sinceState"),
            "token {bad:?} produced {msg:?}",
        );
    }
}

#[test]
fn mailbox_changes_future_since_state_lifecycle_half() {
    let (_d, _mds, store) = open_mailstore();
    let _set = provision(&store);
    let err = store
        .mailbox_changes(TEST_ACCOUNT, "999:0", 100)
        .expect_err("future lifecycle half should be rejected");
    assert!(err.to_string().starts_with("future since_state"));
}

#[test]
fn mailbox_changes_future_since_state_set_change_half() {
    let (_d, _mds, store) = open_mailstore();
    let _set = provision(&store);
    let err = store
        .mailbox_changes(TEST_ACCOUNT, "0:999", 100)
        .expect_err("future set_change half should be rejected");
    assert!(err.to_string().starts_with("future since_state"));
}

#[test]
fn mailbox_changes_zero_token_against_unprovisioned_account_succeeds_empty() {
    let (_d, _mds, store) = open_mailstore();
    let cs = store.mailbox_changes(TEST_ACCOUNT, "0:0", 100).unwrap();
    assert_eq!(cs.new_state, "0:0");
    assert!(!cs.has_more_changes);
    assert!(cs.created.is_empty() && cs.updated.is_empty() && cs.destroyed.is_empty());
    assert!(cs.updated_properties.is_none());
}

#[test]
fn mailbox_changes_nonzero_token_against_unprovisioned_account_is_future() {
    let (_d, _mds, store) = open_mailstore();
    let err = store
        .mailbox_changes(TEST_ACCOUNT, "1:0", 100)
        .expect_err("nonzero token against unprovisioned set should be future");
    assert!(err.to_string().starts_with("future since_state"));
}

#[test]
fn mailbox_changes_create_only_yields_created() {
    let (_d, _mds, store) = open_mailstore();
    let _set = provision(&store);
    let id = store
        .create_mailbox(TEST_ACCOUNT, "Inbox", None, default_attrs())
        .unwrap();
    let cs = store.mailbox_changes(TEST_ACCOUNT, "0:0", 100).unwrap();
    assert_eq!(cs.created, vec![id]);
    assert!(cs.updated.is_empty());
    assert!(cs.destroyed.is_empty());
    assert!(!cs.has_more_changes);
}

#[test]
fn mailbox_changes_create_then_destroy_within_window_is_phantom() {
    let (_d, _mds, store) = open_mailstore();
    let _set = provision(&store);
    let id = store
        .create_mailbox(TEST_ACCOUNT, "Scratch", None, default_attrs())
        .unwrap();
    store
        .destroy_mailbox(TEST_ACCOUNT, id, DestroyPolicy::Reject)
        .unwrap();
    let cs = store.mailbox_changes(TEST_ACCOUNT, "0:0", 100).unwrap();
    // Phantom: client never observed the mailbox, so it appears in
    // no array (RFC 8621 §2.6).
    assert!(cs.created.is_empty(), "phantom must not appear in created");
    assert!(cs.updated.is_empty(), "phantom must not appear in updated");
    assert!(
        cs.destroyed.is_empty(),
        "phantom must not appear in destroyed"
    );
}

#[test]
fn mailbox_changes_destroy_of_pre_existing_mailbox_yields_destroyed() {
    let (_d, _mds, store) = open_mailstore();
    let _set = provision(&store);
    let id = store
        .create_mailbox(TEST_ACCOUNT, "Scratch", None, default_attrs())
        .unwrap();
    // Snapshot state token *after* the create so the destroy lands
    // strictly inside the next window.
    let mid_state = store.mailbox_state(TEST_ACCOUNT).unwrap();
    store
        .destroy_mailbox(TEST_ACCOUNT, id, DestroyPolicy::Reject)
        .unwrap();
    let cs = store
        .mailbox_changes(TEST_ACCOUNT, &mid_state, 100)
        .unwrap();
    assert_eq!(cs.destroyed, vec![id]);
    assert!(cs.created.is_empty() && cs.updated.is_empty());
}

#[test]
fn mailbox_changes_update_only_yields_updated_with_null_updated_properties() {
    let (_d, _mds, store) = open_mailstore();
    let _set = provision(&store);
    let id = store
        .create_mailbox(TEST_ACCOUNT, "Scratch", None, default_attrs())
        .unwrap();
    let mid_state = store.mailbox_state(TEST_ACCOUNT).unwrap();
    // update_mailbox with only an isSubscribed change → emits
    // CONTAINER_ATTRS_CHANGED (no count signal).
    store
        .update_mailbox(TEST_ACCOUNT, id, None, None, None, None, Some(true))
        .unwrap();
    let cs = store
        .mailbox_changes(TEST_ACCOUNT, &mid_state, 100)
        .unwrap();
    assert_eq!(cs.updated, vec![id]);
    assert!(cs.created.is_empty() && cs.destroyed.is_empty());
    assert!(
        cs.updated_properties.is_none(),
        "Phase 2 always returns None (= JMAP null projection)"
    );
}

#[test]
fn mailbox_changes_pagination_truncates_lifecycle_only_and_freezes_set_change() {
    let (_d, _mds, store) = open_mailstore();
    let _set = provision(&store);
    let a = store
        .create_mailbox(TEST_ACCOUNT, "A", None, default_attrs())
        .unwrap();
    let _b = store
        .create_mailbox(TEST_ACCOUNT, "B", None, default_attrs())
        .unwrap();
    // max_changes=1 → first page truncates. has_more_changes=true,
    // new_state advances lifecycle half only.
    let cs = store.mailbox_changes(TEST_ACCOUNT, "0:0", 1).unwrap();
    assert!(cs.has_more_changes);
    assert_eq!(cs.created, vec![a]);
    let (l, s) = parse_paired(&cs.new_state);
    assert!(l > 0, "lifecycle half advances on truncated page");
    assert_eq!(
        s, 0,
        "set_change half stays at since_state on truncated page"
    );

    // Next page from the returned token returns the rest, drained, both halves advance.
    let cs2 = store
        .mailbox_changes(TEST_ACCOUNT, &cs.new_state, 100)
        .unwrap();
    assert!(!cs2.has_more_changes);
    assert_eq!(cs2.created.len(), 1);
}

#[test]
fn mailbox_changes_final_page_advances_both_halves() {
    let (_d, _mds, store) = open_mailstore();
    let _set = provision(&store);
    let _a = store
        .create_mailbox(TEST_ACCOUNT, "A", None, default_attrs())
        .unwrap();
    let cs = store.mailbox_changes(TEST_ACCOUNT, "0:0", 100).unwrap();
    assert!(!cs.has_more_changes);
    let (l, _s) = parse_paired(&cs.new_state);
    assert!(l > 0);
    // mailbox_state advances strictly identically on the final page:
    // the substrate is authoritative for both reads inside the same
    // tx, but a follow-up snapshot must not drift.
    assert_eq!(cs.new_state, store.mailbox_state(TEST_ACCOUNT).unwrap());
}

#[test]
fn mailbox_changes_count_synthesis_surfaces_count_only_container_as_updated() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = store
        .create_mailbox(
            TEST_ACCOUNT,
            "Inbox",
            None,
            ContainerAttrs {
                special_use: Some("\\Inbox".into()),
                subscribed: false,
                extra: serde_json::json!({}),
            },
        )
        .unwrap();
    let mid_state = store.mailbox_state(TEST_ACCOUNT).unwrap();
    // Add a message to Inbox — no lifecycle row, but the
    // set_change stream records the membership add. Count synthesis
    // should surface Inbox as `updated` with `updated_properties = None`.
    let _item = add_item_in(&mds, &set, inbox, b"hello");
    let cs = store
        .mailbox_changes(TEST_ACCOUNT, &mid_state, 100)
        .unwrap();
    assert_eq!(cs.updated, vec![inbox]);
    assert!(cs.created.is_empty() && cs.destroyed.is_empty());
    assert!(cs.updated_properties.is_none());
}

#[test]
fn mailbox_changes_count_synthesis_ignores_staging_container() {
    let (_d, _mds, store) = open_mailstore();
    let _set = provision(&store);
    // create_blob_ref provisions __upload_staging__ + writes an item
    // there. No lifecycle event surfaces for the staging container
    // (filter applied in mailbox_changes); count synthesis must
    // also skip it.
    let payload = b"a";
    let h = store.put_blob(payload).unwrap();
    let _alias = store
        .create_blob_ref(TEST_ACCOUNT, h, payload.len() as u64, 9_999_999_999)
        .unwrap();
    let cs = store.mailbox_changes(TEST_ACCOUNT, "0:0", 100).unwrap();
    // No JMAP-visible mailbox changed.
    assert!(
        cs.created.is_empty(),
        "staging container leaked into created"
    );
    assert!(
        cs.updated.is_empty(),
        "staging container leaked into updated"
    );
    assert!(
        cs.destroyed.is_empty(),
        "staging container leaked into destroyed"
    );
}

#[test]
fn mailbox_changes_lifecycle_wins_over_count_for_created_with_membership() {
    // A mailbox is created AND receives a message in the same
    // window. Lifecycle classification says `created`; count
    // synthesis would also fire. The precedence rule says
    // `created` only, not both.
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let folder = store
        .create_mailbox(TEST_ACCOUNT, "Folder", None, default_attrs())
        .unwrap();
    let _item = add_item_in(&mds, &set, folder, b"hi");
    let cs = store.mailbox_changes(TEST_ACCOUNT, "0:0", 100).unwrap();
    assert_eq!(cs.created, vec![folder]);
    assert!(
        !cs.updated.contains(&folder),
        "created should win; folder must not also appear as updated"
    );
}

#[test]
fn mailbox_changes_max_zero_is_empty_page_with_progress_flag() {
    // `max_changes == 0` is a zero-cost "anything pending?" probe.
    // The trait must NOT silently coerce to 1 (which would emit
    // an un-asked-for row and break the storage-layer contract
    // with whatever caller passes 0 — JMAP rejects 0 upstream per
    // RFC 8620 §5.2 PositiveInt, but the trait is internal).
    let (_d, _mds, store) = open_mailstore();
    let _set = provision(&store);
    // No events yet → empty page, no progress.
    let cs = store.mailbox_changes(TEST_ACCOUNT, "0:0", 0).unwrap();
    assert_eq!(cs.new_state, "0:0");
    assert!(!cs.has_more_changes);
    assert!(cs.created.is_empty() && cs.updated.is_empty() && cs.destroyed.is_empty());

    // After a real mailbox create, the lifecycle head advances and
    // a max=0 probe reports has_more_changes without surfacing any
    // ids.
    store
        .create_mailbox(
            TEST_ACCOUNT,
            "Folder",
            None,
            ContainerAttrs {
                special_use: None,
                subscribed: false,
                extra: serde_json::json!({}),
            },
        )
        .unwrap();
    let cs = store.mailbox_changes(TEST_ACCOUNT, "0:0", 0).unwrap();
    assert_eq!(
        cs.new_state, "0:0",
        "zero-page must pin new_state at since_state"
    );
    assert!(cs.has_more_changes, "head moved → has_more should be true");
    assert!(cs.created.is_empty(), "zero-page must not emit any ids");
    assert!(cs.updated.is_empty());
    assert!(cs.destroyed.is_empty());
}

#[test]
fn mailbox_changes_idempotent_zero_token_under_no_events() {
    let (_d, _mds, store) = open_mailstore();
    let _set = provision(&store);
    let cs = store.mailbox_changes(TEST_ACCOUNT, "0:0", 100).unwrap();
    assert_eq!(cs.new_state, "0:0");
    assert!(!cs.has_more_changes);
    assert!(cs.created.is_empty() && cs.updated.is_empty() && cs.destroyed.is_empty());
}

// ---- list_thread (Task 1.12) -----------------------------------------------

/// Insert a `mail_threads` row directly. The Phase 1 MailStore surface
/// does not (yet) own thread membership — `db::thread::find_or_create`
/// does, behind the JMAP delivery path. These tests bypass that wiring
/// and write the sidecar rows raw so list_thread is exercised in
/// isolation, matching the spec line "read from `mail_threads` sidecar."
fn insert_thread(
    mds: &SqliteCasMds,
    set: &SetId,
    item: &ItemId,
    account: AccountId,
    thread_id: &str,
) {
    mds.with_set_tx(set, |tx| {
        tx.tx()
            .execute(
                "INSERT INTO mail_threads (item_id, account_id, thread_id) \
                 VALUES (?1, ?2, ?3)",
                params![item.0.to_string(), account, thread_id],
            )
            .unwrap();
        Ok(())
    })
    .unwrap();
}

fn env_with_date(date: i64, message_id: &str) -> EmailEnvelope {
    EmailEnvelope {
        from: "alice@example.com".into(),
        to: vec!["bob@example.com".into()],
        cc: Vec::new(),
        bcc: Vec::new(),
        reply_to: Vec::new(),
        subject: "re: lunch".into(),
        date,
        message_id: Some(message_id.into()),
    }
}

#[test]
fn list_thread_three_emails_same_chain_returned_in_date_order() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    // Three emails sharing a Message-ID/References chain → same
    // thread. Insert envelopes with unsorted `date_ts` to confirm the
    // result is sorted at read time, not by insertion order.
    let (h1, _) = put_blob(&mds, b"first");
    let m1 = Membership {
        container: inbox,
        flags: Flags(0),
        added_at: 0,
    };
    let id1 = mds.add_item(&set, &h1, &[m1]).unwrap().item_id;
    insert_envelope(
        &mds,
        &set,
        &id1,
        &env_with_date(1_700_000_300, "<msg-c@example.com>"),
    );

    let (h2, _) = put_blob(&mds, b"second");
    let m2 = Membership {
        container: inbox,
        flags: Flags(0),
        added_at: 0,
    };
    let id2 = mds.add_item(&set, &h2, &[m2]).unwrap().item_id;
    insert_envelope(
        &mds,
        &set,
        &id2,
        &env_with_date(1_700_000_100, "<msg-a@example.com>"),
    );

    let (h3, _) = put_blob(&mds, b"third");
    let m3 = Membership {
        container: inbox,
        flags: Flags(0),
        added_at: 0,
    };
    let id3 = mds.add_item(&set, &h3, &[m3]).unwrap().item_id;
    insert_envelope(
        &mds,
        &set,
        &id3,
        &env_with_date(1_700_000_200, "<msg-b@example.com>"),
    );

    let tid = "thread-xyz";
    insert_thread(&mds, &set, &id1, TEST_ACCOUNT, tid);
    insert_thread(&mds, &set, &id2, TEST_ACCOUNT, tid);
    insert_thread(&mds, &set, &id3, TEST_ACCOUNT, tid);

    let out = store
        .list_thread(TEST_ACCOUNT, ThreadId(tid.into()))
        .unwrap();
    // Sorted by date_ts ascending: id2 (100) < id3 (200) < id1 (300).
    assert_eq!(out, vec![id2, id3, id1]);
}

#[test]
fn list_thread_unknown_thread_returns_empty_vec() {
    let (_d, _mds, store) = open_mailstore();
    let _set = provision(&store);

    let out = store
        .list_thread(TEST_ACCOUNT, ThreadId("never-existed".into()))
        .unwrap();
    assert!(out.is_empty());
}

#[test]
fn list_thread_filters_by_account() {
    // mail_threads is keyed by (item_id, account_id) — a thread row
    // for another account on the same item must NOT surface.
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    let (h, _) = put_blob(&mds, b"shared");
    let m = Membership {
        container: inbox,
        flags: Flags(0),
        added_at: 0,
    };
    let id = mds.add_item(&set, &h, &[m]).unwrap().item_id;
    insert_envelope(
        &mds,
        &set,
        &id,
        &env_with_date(1_700_000_000, "<msg-shared@example.com>"),
    );

    // Insert thread row for a different account.
    insert_thread(&mds, &set, &id, TEST_ACCOUNT + 1, "other-thread");

    let out = store
        .list_thread(TEST_ACCOUNT, ThreadId("other-thread".into()))
        .unwrap();
    assert!(out.is_empty(), "got {out:?}");
}

#[test]
fn list_thread_no_dangling_rows_after_v1_8_cascade() {
    // Pre-v1.8 `mail_threads` had no FK to item(id), so a dangling row
    // (item reaped, thread row left behind) was possible and had to be
    // filtered at read time. v1.8 added `REFERENCES item(id) ON DELETE
    // CASCADE`, which makes that state unreachable two ways:
    //   (a) you cannot INSERT a thread row for an item that does not
    //       exist (the FK rejects it), and
    //   (b) deleting the item reaps its thread row.
    // Both are asserted here; the read path's `JOIN item` is now a
    // debug invariant rather than a load-bearing filter.
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    let (h, _) = put_blob(&mds, b"present");
    let m = Membership {
        container: inbox,
        flags: Flags(0),
        added_at: 0,
    };
    let id_real = mds.add_item(&set, &h, &[m]).unwrap().item_id;
    insert_envelope(
        &mds,
        &set,
        &id_real,
        &env_with_date(1_700_000_000, "<msg-real@example.com>"),
    );

    let tid = "thread-no-ghost";
    insert_thread(&mds, &set, &id_real, TEST_ACCOUNT, tid);

    // (a) Inserting a thread row for a non-existent item is now rejected.
    let id_phantom = ItemId(uuid::Uuid::new_v4());
    let fk_err: cosmix_mds::Result<()> = mds.with_set_tx(&set, |tx| {
        tx.tx()
            .execute(
                "INSERT INTO mail_threads (item_id, account_id, thread_id) \
                 VALUES (?1, ?2, ?3)",
                params![id_phantom.0.to_string(), TEST_ACCOUNT, tid],
            )
            .map_err(|e| cosmix_mds::Error::Other(format!("fk probe: {e}")))?;
        Ok(())
    });
    assert!(
        fk_err.is_err(),
        "FK must reject a mail_threads row for a non-existent item"
    );

    // The real row is still the only member, returned cleanly.
    let out = store
        .list_thread(TEST_ACCOUNT, ThreadId(tid.into()))
        .unwrap();
    assert_eq!(out, vec![id_real]);

    // (b) Deleting the item reaps its thread row via the cascade.
    store.delete_email(TEST_ACCOUNT, id_real).unwrap();
    let out_after = store
        .list_thread(TEST_ACCOUNT, ThreadId(tid.into()))
        .unwrap();
    assert!(out_after.is_empty(), "cascade must reap the thread row");
}

#[test]
fn list_thread_falls_back_to_received_at_for_items_without_envelopes() {
    // Items predating Task 1.7's create_email path lack a
    // mail_envelopes row; they must still appear in the result so
    // the read path doesn't silently drop pre-Phase-A messages.
    // They sort *after* envelope-bearing rows (by `received_at` ms),
    // matching the documented contract.
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    let (h_env, _) = put_blob(&mds, b"with-envelope");
    let m1 = Membership {
        container: inbox,
        flags: Flags(0),
        added_at: 0,
    };
    let id_env = mds.add_item(&set, &h_env, &[m1]).unwrap().item_id;
    insert_envelope(
        &mds,
        &set,
        &id_env,
        &env_with_date(1_700_000_500, "<msg-env@example.com>"),
    );

    let (h_bare1, _) = put_blob(&mds, b"no-envelope-1");
    let m2 = Membership {
        container: inbox,
        flags: Flags(0),
        added_at: 0,
    };
    let id_bare1 = mds.add_item(&set, &h_bare1, &[m2]).unwrap().item_id;
    set_received_at(&mds, &set, &id_bare1, 2_000_000_000);

    let (h_bare2, _) = put_blob(&mds, b"no-envelope-2");
    let m3 = Membership {
        container: inbox,
        flags: Flags(0),
        added_at: 0,
    };
    let id_bare2 = mds.add_item(&set, &h_bare2, &[m3]).unwrap().item_id;
    set_received_at(&mds, &set, &id_bare2, 1_500_000_000);

    let tid = "mixed-thread";
    insert_thread(&mds, &set, &id_env, TEST_ACCOUNT, tid);
    insert_thread(&mds, &set, &id_bare1, TEST_ACCOUNT, tid);
    insert_thread(&mds, &set, &id_bare2, TEST_ACCOUNT, tid);

    let out = store
        .list_thread(TEST_ACCOUNT, ThreadId(tid.into()))
        .unwrap();
    // Envelope row first (has date_ts), then bare rows ordered by
    // `item.received_at` ASC within the bare bucket: bare2 (1.5e9)
    // before bare1 (2.0e9). Confirms the secondary sort key on the
    // CASE-WHEN bucket-1 partition.
    assert_eq!(out, vec![id_env, id_bare2, id_bare1]);
}

#[test]
fn list_thread_unprovisioned_account_errors_with_mailbox_not_found() {
    let (_d, _mds, store) = open_mailstore();
    // No `provision` — set has not been bootstrapped.
    let err = store
        .list_thread(TEST_ACCOUNT, ThreadId("anything".into()))
        .unwrap_err();
    let msg = format!("{err}");
    assert!(msg.starts_with("mailbox not found"), "got: {msg}");
}

#[test]
fn list_thread_breaks_ties_on_item_id_for_determinism() {
    // Two thread members with identical date_ts — the order must
    // still be deterministic across runs so JMAP `Thread.emailIds`
    // is reproducible. Tie-breaker is `item.id` ASC (lexicographic
    // on the UUID string).
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    let same_date = 1_700_000_000;

    let (h1, _) = put_blob(&mds, b"first");
    let m1 = Membership {
        container: inbox,
        flags: Flags(0),
        added_at: 0,
    };
    let id1 = mds.add_item(&set, &h1, &[m1]).unwrap().item_id;
    insert_envelope(
        &mds,
        &set,
        &id1,
        &env_with_date(same_date, "<msg-1@example.com>"),
    );

    let (h2, _) = put_blob(&mds, b"second");
    let m2 = Membership {
        container: inbox,
        flags: Flags(0),
        added_at: 0,
    };
    let id2 = mds.add_item(&set, &h2, &[m2]).unwrap().item_id;
    insert_envelope(
        &mds,
        &set,
        &id2,
        &env_with_date(same_date, "<msg-2@example.com>"),
    );

    let tid = "tie-thread";
    insert_thread(&mds, &set, &id1, TEST_ACCOUNT, tid);
    insert_thread(&mds, &set, &id2, TEST_ACCOUNT, tid);

    let out = store
        .list_thread(TEST_ACCOUNT, ThreadId(tid.into()))
        .unwrap();
    let mut expected = vec![id1, id2];
    expected.sort_by_key(|i| i.0.to_string());
    assert_eq!(out, expected);
}

// -------------------- Task 4.1: list_threads --------------------

#[test]
fn list_threads_returns_distinct_thread_ids_present_in_mail_threads() {
    // Two threads with two members each, plus one item with no
    // mail_threads row at all (must be excluded). The primitive
    // collapses members to one row per thread_id (DISTINCT) and
    // skips threads whose every member has been reaped.
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    let id_a1 = add_item_in(&mds, &set, inbox, b"a1");
    let id_a2 = add_item_in(&mds, &set, inbox, b"a2");
    let id_b1 = add_item_in(&mds, &set, inbox, b"b1");
    let id_b2 = add_item_in(&mds, &set, inbox, b"b2");
    // No mail_threads row for this one — list_threads must not
    // surface a phantom thread for it.
    let _id_orphan = add_item_in(&mds, &set, inbox, b"orphan");

    insert_thread(&mds, &set, &id_a1, TEST_ACCOUNT, "thread-a");
    insert_thread(&mds, &set, &id_a2, TEST_ACCOUNT, "thread-a");
    insert_thread(&mds, &set, &id_b1, TEST_ACCOUNT, "thread-b");
    insert_thread(&mds, &set, &id_b2, TEST_ACCOUNT, "thread-b");

    let out = store.list_threads(TEST_ACCOUNT, 0, 100).unwrap();
    let ids: Vec<String> = out.into_iter().map(|t| t.0).collect();
    assert_eq!(ids, vec!["thread-a", "thread-b"]);
}

#[test]
fn list_threads_respects_position_and_limit() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    for tid in ["t-1", "t-2", "t-3", "t-4"] {
        let id = add_item_in(&mds, &set, inbox, tid.as_bytes());
        insert_thread(&mds, &set, &id, TEST_ACCOUNT, tid);
    }

    // Page 1: first two.
    let p1 = store.list_threads(TEST_ACCOUNT, 0, 2).unwrap();
    assert_eq!(
        p1.into_iter().map(|t| t.0).collect::<Vec<_>>(),
        vec!["t-1", "t-2"]
    );

    // Page 2: skip 2, take 2.
    let p2 = store.list_threads(TEST_ACCOUNT, 2, 2).unwrap();
    assert_eq!(
        p2.into_iter().map(|t| t.0).collect::<Vec<_>>(),
        vec!["t-3", "t-4"]
    );

    // Past-the-end position returns empty (not an error).
    let p3 = store.list_threads(TEST_ACCOUNT, 100, 100).unwrap();
    assert!(p3.is_empty());
}

#[test]
fn list_threads_disappear_when_every_item_reaped_via_cascade() {
    // v1.8: `mail_threads.item_id` FK-cascades on item delete, so a
    // thread whose every member item is deleted observably disappears
    // from `list_threads`. (Pre-v1.8 this relied on a read-time join
    // filtering dangling rows; now the rows are physically reaped.)
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    // Two live threads, each with a real item.
    let (h_live, _) = put_blob(&mds, b"live");
    let (id_live, _) = store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            h_live,
            sample_envelope(),
            &[],
            Flags(0),
            Tags::new(),
            1_730_000_000_000,
        )
        .unwrap();
    // Force a deterministic thread id for the live email.
    mds.with_set_tx(&set, |tx| {
        tx.tx()
            .execute(
                "UPDATE mail_threads SET thread_id = 'thread-live' WHERE item_id = ?1",
                params![id_live.0.to_string()],
            )
            .unwrap();
        Ok(())
    })
    .unwrap();

    let (h_doomed, _) = put_blob(&mds, b"doomed");
    let (id_doomed, _) = store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            h_doomed,
            env_with_date(1_700_000_000, "<doomed@example.com>"),
            &[],
            Flags(0),
            Tags::new(),
            1_730_000_000_001,
        )
        .unwrap();
    mds.with_set_tx(&set, |tx| {
        tx.tx()
            .execute(
                "UPDATE mail_threads SET thread_id = 'thread-doomed' WHERE item_id = ?1",
                params![id_doomed.0.to_string()],
            )
            .unwrap();
        Ok(())
    })
    .unwrap();

    // Both threads visible before deletion.
    let before: Vec<String> = store
        .list_threads(TEST_ACCOUNT, 0, 100)
        .unwrap()
        .into_iter()
        .map(|t| t.0)
        .collect();
    assert_eq!(before, vec!["thread-doomed", "thread-live"]);

    // Delete the doomed email → its mail_threads row cascades away.
    store.delete_email(TEST_ACCOUNT, id_doomed).unwrap();

    let after: Vec<String> = store
        .list_threads(TEST_ACCOUNT, 0, 100)
        .unwrap()
        .into_iter()
        .map(|t| t.0)
        .collect();
    assert_eq!(after, vec!["thread-live"]);
}

#[test]
fn list_threads_unprovisioned_account_returns_empty_vec() {
    // Mirrors `email_state` / `email_changes(0)` zero-baseline:
    // an unprovisioned account is "no threads", not an error.
    let (_d, _mds, store) = open_mailstore();
    let out = store.list_threads(TEST_ACCOUNT, 0, 100).unwrap();
    assert!(out.is_empty());
}

// -------------------- Task 4.1a: mail_threads write on create --------------------

/// `create_email` populates `mail_threads` so the freshly-minted item
/// is reachable through `Thread/get`. Without an in-reply-to match a
/// new UUIDv4 thread id is allocated; the row joins to the same item
/// the caller just received back.
#[test]
fn create_email_populates_mail_threads_with_fresh_thread_id_when_no_reply_match() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    let (hash, _) = put_blob(&mds, b"first");
    let (id, _uid) = store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            hash,
            sample_envelope(),
            &[],
            Flags(0),
            Tags::new(),
            1_700_000_000_000,
        )
        .unwrap();

    // list_threads must surface a single thread (the newly-allocated
    // one), and list_thread on that thread must return our item.
    let threads = store.list_threads(TEST_ACCOUNT, 0, 100).unwrap();
    assert_eq!(threads.len(), 1, "exactly one thread post-create");
    let tid = threads.into_iter().next().unwrap();
    let members = store.list_thread(TEST_ACCOUNT, tid.clone()).unwrap();
    assert_eq!(members, vec![id]);

    // The thread id MUST be a parseable UUIDv4 — the absence of an
    // in-reply-to match falls through to `Uuid::new_v4()`.
    Uuid::parse_str(&tid.0).expect("thread_id should be UUIDv4");
}

/// `create_email` reuses an existing `thread_id` when `in_reply_to`
/// names a message-id already present on a `mail_envelopes` row in
/// the same account. Mirrors legacy `db::thread::find_or_create`.
#[test]
fn create_email_reuses_thread_id_when_in_reply_to_matches_existing() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    // Parent email: message-id "<root@example.com>".
    let (h_root, _) = put_blob(&mds, b"root");
    let parent_env = EmailEnvelope {
        from: "alice@example.com".into(),
        to: vec!["bob@example.com".into()],
        cc: Vec::new(),
        bcc: Vec::new(),
        reply_to: Vec::new(),
        subject: "topic".into(),
        date: 1_700_000_000,
        message_id: Some("<root@example.com>".into()),
    };
    let (parent_id, _parent_uid) = store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            h_root,
            parent_env,
            &[],
            Flags(0),
            Tags::new(),
            1_700_000_000_000,
        )
        .unwrap();

    let parent_threads = store.list_threads(TEST_ACCOUNT, 0, 100).unwrap();
    assert_eq!(parent_threads.len(), 1);
    let parent_tid = parent_threads.into_iter().next().unwrap();

    // Reply email: in_reply_to references the parent's message_id.
    let (h_reply, _) = put_blob(&mds, b"reply");
    let reply_env = EmailEnvelope {
        from: "bob@example.com".into(),
        to: vec!["alice@example.com".into()],
        cc: Vec::new(),
        bcc: Vec::new(),
        reply_to: Vec::new(),
        subject: "Re: topic".into(),
        date: 1_700_000_100,
        message_id: Some("<reply@example.com>".into()),
    };
    let (reply_id, _reply_uid) = store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            h_reply,
            reply_env,
            &["<root@example.com>".to_string()],
            Flags(0),
            Tags::new(),
            1_700_000_100_000,
        )
        .unwrap();

    // Still exactly one thread post-reply, and it covers both items.
    let threads = store.list_threads(TEST_ACCOUNT, 0, 100).unwrap();
    assert_eq!(threads, vec![parent_tid.clone()]);
    let members = store.list_thread(TEST_ACCOUNT, parent_tid).unwrap();
    let mut expected = vec![parent_id, reply_id];
    expected.sort_by_key(|i| i.0.to_string());
    let mut got = members.clone();
    got.sort_by_key(|i| i.0.to_string());
    assert_eq!(got, expected);
}

/// In-reply-to references that match no known message-id fall through
/// to a fresh thread id — silent skip, not an error.
#[test]
fn create_email_unknown_in_reply_to_falls_through_to_new_thread() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    let (hash, _) = put_blob(&mds, b"orphan-reply");
    let (_id, _uid) = store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            hash,
            sample_envelope(),
            &["<no-such-message@example.com>".to_string()],
            Flags(0),
            Tags::new(),
            1_700_000_000_000,
        )
        .unwrap();

    let threads = store.list_threads(TEST_ACCOUNT, 0, 100).unwrap();
    assert_eq!(threads.len(), 1, "unmatched ref → fresh thread");
    Uuid::parse_str(&threads[0].0).expect("thread_id should be UUIDv4");
}

/// `create_email_with_memberships` populates `mail_threads` exactly
/// once per item, regardless of how many memberships the item has —
/// `mail_threads` is keyed on (item_id, account_id), not (item_id,
/// container_id).
#[test]
fn create_email_with_memberships_populates_mail_threads_once_per_item() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));
    let archive = mk_container(&mds, &set, "Archive", None);

    let (hash, _) = put_blob(&mds, b"multi-mailbox");
    let (id, _uids) = store
        .create_email_with_memberships(
            TEST_ACCOUNT,
            &[inbox, archive],
            hash,
            sample_envelope(),
            &[],
            Flags(0),
            Tags::new(),
            1_700_000_000_000,
        )
        .unwrap();

    let threads = store.list_threads(TEST_ACCOUNT, 0, 100).unwrap();
    assert_eq!(
        threads.len(),
        1,
        "one mail_threads row even with 2 memberships"
    );
    let members = store.list_thread(TEST_ACCOUNT, threads[0].clone()).unwrap();
    assert_eq!(members, vec![id]);
}

/// Cross-account isolation: a reply in account A whose `in_reply_to`
/// matches a message-id stored under account B MUST NOT join account
/// B's thread. The lookup is per-account by construction (`SetId` is
/// per-account; the `with_set_tx` callback only sees this account's
/// `mail_envelopes` / `mail_threads` rows).
#[test]
fn create_email_in_reply_to_lookup_is_per_account() {
    let (_d, mds, store) = open_mailstore();
    let set_a = provision(&store);
    let inbox_a = mk_container(&mds, &set_a, "Inbox", Some("\\Inbox"));

    let other_account: AccountId = TEST_ACCOUNT + 1;
    let set_b = store.ensure_account_set(other_account).unwrap();
    let inbox_b = mk_container(&mds, &set_b, "Inbox", Some("\\Inbox"));

    // Account B has the parent message.
    let (h_b, _) = put_blob(&mds, b"parent-in-b");
    let parent_env = EmailEnvelope {
        from: "alice@example.com".into(),
        to: vec!["bob@example.com".into()],
        cc: Vec::new(),
        bcc: Vec::new(),
        reply_to: Vec::new(),
        subject: "B".into(),
        date: 1_700_000_000,
        message_id: Some("<cross@example.com>".into()),
    };
    let (_parent_b, _parent_b_uid) = store
        .create_email(
            other_account,
            inbox_b,
            h_b,
            parent_env,
            &[],
            Flags(0),
            Tags::new(),
            1_700_000_000_000,
        )
        .unwrap();

    // Account A creates a reply *referencing* B's message-id.
    let (h_a, _) = put_blob(&mds, b"reply-in-a");
    let reply_env = EmailEnvelope {
        from: "carol@example.com".into(),
        to: vec!["dave@example.com".into()],
        cc: Vec::new(),
        bcc: Vec::new(),
        reply_to: Vec::new(),
        subject: "Re: B".into(),
        date: 1_700_000_100,
        message_id: Some("<cross-reply@example.com>".into()),
    };
    let (_reply_a, _reply_a_uid) = store
        .create_email(
            TEST_ACCOUNT,
            inbox_a,
            h_a,
            reply_env,
            &["<cross@example.com>".to_string()],
            Flags(0),
            Tags::new(),
            1_700_000_100_000,
        )
        .unwrap();

    // Account A must see exactly one thread (its own fresh one), not
    // join B's.
    let threads_a = store.list_threads(TEST_ACCOUNT, 0, 100).unwrap();
    assert_eq!(threads_a.len(), 1, "A must not join B's thread");

    let threads_b = store.list_threads(other_account, 0, 100).unwrap();
    assert_eq!(threads_b.len(), 1, "B retains its solo thread");

    // The two thread ids must be disjoint.
    assert_ne!(threads_a[0].0, threads_b[0].0);
}

// -------------------- Phase 8e gate 3: mail_search rebuild --------------------

/// Probe helper — wipe all `mail_search` rows for one account in a
/// committing transaction (simulates "missing baseline"). Unlike
/// `count_mail_search_*` helpers above, this one needs to commit so
/// the rebuild path sees the wiped state when it re-enters the same
/// per-set DB.
fn wipe_mail_search(mds: &SqliteCasMds, set: &SetId, account: AccountId) {
    mds.with_set_tx(set, |tx| {
        tx.tx()
            .execute(
                "DELETE FROM mail_search WHERE account_id = ?1",
                params![account],
            )
            .map_err(|e| cosmix_mds::Error::Other(format!("wipe: {e}")))?;
        Ok(())
    })
    .unwrap();
}

/// Bit-for-bit reproduction: after `rebuild_search_index`, every
/// MATCH query that hit a row on the original create_email-time
/// projection must hit the same row again. Verifies columns drawn
/// from all four projection inputs (subject, body_text, headers,
/// normalized_addrs).
#[test]
fn rebuild_search_index_reproduces_baseline_bit_for_bit() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    let raw1 = b"From: alice@example.com\r\nTo: bob@example.com\r\nSubject: lunch\r\nContent-Type: text/plain\r\n\r\nshall we meet at the cafe today\r\n";
    let raw2 = b"From: carol@example.com\r\nTo: dave@example.com\r\nSubject: pelican project\r\nContent-Type: text/plain\r\n\r\nstatus update on the new system\r\n";
    let raw3 = b"From: eve@kestrel-domain-xyz.test\r\nTo: bob@example.com\r\nSubject: quarterly review\r\nContent-Type: text/plain\r\n\r\nplease respond by friday\r\n";
    let (h1, _) = put_blob(&mds, raw1);
    let (h2, _) = put_blob(&mds, raw2);
    let (h3, _) = put_blob(&mds, raw3);

    let env1 = EmailEnvelope {
        from: "alice@example.com".into(),
        to: vec!["bob@example.com".into()],
        cc: Vec::new(),
        bcc: Vec::new(),
        reply_to: Vec::new(),
        subject: "lunch".into(),
        date: 1_700_000_000,
        message_id: Some("<m1@example.com>".into()),
    };
    let env2 = EmailEnvelope {
        from: "carol@example.com".into(),
        to: vec!["dave@example.com".into()],
        cc: Vec::new(),
        bcc: Vec::new(),
        reply_to: Vec::new(),
        subject: "pelican project".into(),
        date: 1_700_000_001,
        message_id: Some("<m2@example.com>".into()),
    };
    let env3 = EmailEnvelope {
        from: "eve@kestrel-domain-xyz.test".into(),
        to: vec!["bob@example.com".into()],
        cc: Vec::new(),
        bcc: Vec::new(),
        reply_to: Vec::new(),
        subject: "quarterly review".into(),
        date: 1_700_000_002,
        message_id: Some("<m3@example.com>".into()),
    };

    let (id1, _) = store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            h1,
            env1,
            &[],
            Flags(0),
            Tags::new(),
            1_700_000_000_000,
        )
        .unwrap();
    let (id2, _) = store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            h2,
            env2,
            &[],
            Flags(0),
            Tags::new(),
            1_700_000_001_000,
        )
        .unwrap();
    let (id3, _) = store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            h3,
            env3,
            &[],
            Flags(0),
            Tags::new(),
            1_700_000_002_000,
        )
        .unwrap();

    // Baseline probes cover all four projection columns.
    let probes: &[(&ItemId, &str)] = &[
        (&id1, "lunch"),                  // subject
        (&id1, "cafe"),                   // body_text
        (&id1, "alice"),                  // headers (From localpart)
        (&id2, "pelican"),                // subject
        (&id2, "status"),                 // body_text
        (&id3, "quarterly"),              // subject
        (&id3, "friday"),                 // body_text
        (&id3, "\"kestrel domain xyz\""), // normalized_addrs phrase
    ];
    for (id, needle) in probes {
        assert_eq!(
            count_mail_search_match(&mds, &set, id, needle),
            1,
            "pre-rebuild probe failed: needle={needle:?} item={id:?}",
        );
    }

    // First rebuild — round-trip from the clean baseline.
    let report = store.rebuild_search_index(TEST_ACCOUNT).unwrap();
    assert_eq!(report.items_rebuilt, 3);
    assert_eq!(report.blob_load_failures, 0);

    for (id, needle) in probes {
        assert_eq!(
            count_mail_search_match(&mds, &set, id, needle),
            1,
            "post-rebuild probe failed: needle={needle:?} item={id:?}",
        );
    }
    assert_eq!(count_mail_search_rows_for_item(&mds, &set, &id1), 1);
    assert_eq!(count_mail_search_rows_for_item(&mds, &set, &id2), 1);
    assert_eq!(count_mail_search_rows_for_item(&mds, &set, &id3), 1);

    // Second rebuild on top of the freshly-rebuilt state — idempotent.
    let report2 = store.rebuild_search_index(TEST_ACCOUNT).unwrap();
    assert_eq!(report2.items_rebuilt, 3);
    assert_eq!(report2.blob_load_failures, 0);
    for (id, needle) in probes {
        assert_eq!(
            count_mail_search_match(&mds, &set, id, needle),
            1,
            "second-rebuild probe failed: needle={needle:?} item={id:?}",
        );
    }
}

/// Missing-baseline recovery: explicit `DELETE FROM mail_search`
/// followed by rebuild restores every row and every MATCH-hit. This
/// is the primary acceptance condition for gate 3 — the rebuild
/// command is meant to be the recovery path when the projection is
/// gone.
#[test]
fn rebuild_search_index_recovers_from_missing_baseline() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    let raw = b"From: alice@example.com\r\nSubject: distinctive_unique_token\r\nContent-Type: text/plain\r\n\r\nbody_unique_marker here\r\n";
    let (hash, _) = put_blob(&mds, raw);
    let (id, _) = store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            hash,
            sample_envelope(),
            &[],
            Flags(0),
            Tags::new(),
            1_700_000_000_000,
        )
        .unwrap();

    // Wipe the projection; the underlying envelope + blob remain.
    wipe_mail_search(&mds, &set, TEST_ACCOUNT);
    assert_eq!(count_mail_search_rows_for_item(&mds, &set, &id), 0);
    // sample_envelope() has subject "lunch?" — MATCH on lunch should
    // now be empty.
    assert_eq!(count_mail_search_match(&mds, &set, &id, "lunch"), 0);

    let report = store.rebuild_search_index(TEST_ACCOUNT).unwrap();
    assert_eq!(report.items_rebuilt, 1);
    assert_eq!(report.blob_load_failures, 0);

    // Subject ("lunch?") and body markers re-indexed.
    assert_eq!(count_mail_search_rows_for_item(&mds, &set, &id), 1);
    assert_eq!(count_mail_search_match(&mds, &set, &id, "lunch"), 1);
    assert_eq!(
        count_mail_search_match(&mds, &set, &id, "body_unique_marker"),
        1
    );
}

/// Corrupted-baseline recovery: a `mail_search` row whose indexed
/// columns disagree with the envelope row is replaced by rebuild.
/// We simulate corruption by DELETing the real row and INSERTing a
/// row with bogus indexed content under the same item_id (FTS5
/// contentless tables don't support UPDATE; DELETE+INSERT is the
/// equivalent mutation shape).
#[test]
fn rebuild_search_index_recovers_from_corrupted_baseline() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    let raw = b"From: alice@example.com\r\nSubject: real_subject_token\r\nContent-Type: text/plain\r\n\r\nreal_body_token here\r\n";
    let (hash, _) = put_blob(&mds, raw);
    let env = EmailEnvelope {
        subject: "real_subject_token".into(),
        ..sample_envelope()
    };
    let (id, _) = store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            hash,
            env,
            &[],
            Flags(0),
            Tags::new(),
            1_700_000_000_000,
        )
        .unwrap();

    // Corrupt the projection: wipe the real row, inject a row with
    // bogus indexed columns under the same item_id.
    mds.with_set_tx(&set, |tx| {
        tx.tx()
            .execute(
                "DELETE FROM mail_search WHERE item_id = ?1",
                params![id.0.to_string()],
            )
            .unwrap();
        tx.tx()
            .execute(
                "INSERT INTO mail_search \
                 (item_id, account_id, headers, subject, body_text, normalized_addrs) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    id.0.to_string(),
                    TEST_ACCOUNT,
                    "corrupt_headers_token",
                    "corrupt_subject_token",
                    "corrupt_body_token",
                    "corrupt_addrs_token",
                ],
            )
            .unwrap();
        Ok(())
    })
    .unwrap();

    // Corruption is observable: MATCH hits the bogus tokens, misses
    // the real ones.
    assert_eq!(
        count_mail_search_match(&mds, &set, &id, "corrupt_subject_token"),
        1,
    );
    assert_eq!(
        count_mail_search_match(&mds, &set, &id, "real_subject_token"),
        0,
    );

    let report = store.rebuild_search_index(TEST_ACCOUNT).unwrap();
    assert_eq!(report.items_rebuilt, 1);

    // Post-rebuild: bogus tokens gone, real tokens back.
    assert_eq!(
        count_mail_search_match(&mds, &set, &id, "corrupt_subject_token"),
        0,
    );
    assert_eq!(
        count_mail_search_match(&mds, &set, &id, "real_subject_token"),
        1,
    );
    assert_eq!(
        count_mail_search_match(&mds, &set, &id, "real_body_token"),
        1,
    );
    assert_eq!(count_mail_search_rows_for_item(&mds, &set, &id), 1);
}

/// Regression pin for the round-1 MAJOR fix: rebuild must wipe rows
/// whose UNINDEXED `account_id` column is corrupt (does not match
/// the account being rebuilt). The earlier
/// `DELETE FROM mail_search WHERE account_id = ?1` predicate trusted
/// the column we are rebuilding — a stale row with the wrong
/// `account_id` would survive the wipe and continue to MATCH after
/// the rebuild. The current implementation issues a bare
/// `DELETE FROM mail_search` inside the per-set tx, which catches
/// the corrupt row. A regression to the predicated DELETE would
/// fail this test.
#[test]
fn rebuild_search_index_wipes_rows_with_wrong_account_id() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    let raw = b"From: alice@example.com\r\nSubject: real_subject_token\r\nContent-Type: text/plain\r\n\r\nreal_body_token here\r\n";
    let (hash, _) = put_blob(&mds, raw);
    let env = EmailEnvelope {
        subject: "real_subject_token".into(),
        ..sample_envelope()
    };
    let (id, _) = store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            hash,
            env,
            &[],
            Flags(0),
            Tags::new(),
            1_700_000_000_000,
        )
        .unwrap();

    // Inject a stale `mail_search` row carrying the WRONG account_id
    // (substrate-level corruption — could come from a botched merge,
    // partial restore, or a future multi-account-per-set bug). The
    // tokens are unique so we can MATCH-probe specifically for this
    // row. We use a fresh item_id so the corrupted row does not
    // collide with the real envelope's row.
    let stale_item_id = "00000000-0000-0000-0000-000000000099";
    let wrong_account: AccountId = 99_999;
    mds.with_set_tx(&set, |tx| {
        tx.tx()
            .execute(
                "INSERT INTO mail_search \
                 (item_id, account_id, headers, subject, body_text, normalized_addrs) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    stale_item_id,
                    wrong_account,
                    "stale_wrong_acct_headers",
                    "stale_wrong_acct_subject_token",
                    "stale_wrong_acct_body",
                    "stale_wrong_acct_addrs",
                ],
            )
            .unwrap();
        Ok(())
    })
    .unwrap();

    // Pre-rebuild observability: the stale token MATCHes (regardless
    // of the wrong account_id, because FTS5 MATCH does not consult
    // UNINDEXED columns).
    let stale_hits_before: i64 = mds
        .with_set_tx(&set, |tx| {
            let n: i64 = tx
                .tx()
                .query_row(
                    "SELECT COUNT(*) FROM mail_search \
                     WHERE mail_search MATCH ?1",
                    params!["stale_wrong_acct_subject_token"],
                    |r| r.get(0),
                )
                .unwrap();
            Ok(n)
        })
        .unwrap();
    assert_eq!(
        stale_hits_before, 1,
        "stale row must be present pre-rebuild"
    );

    // Rebuild. Under the bare DELETE this wipes the stale row; under
    // the (rejected) predicated DELETE it would survive.
    let report = store.rebuild_search_index(TEST_ACCOUNT).unwrap();
    assert_eq!(report.items_rebuilt, 1);

    let stale_hits_after: i64 = mds
        .with_set_tx(&set, |tx| {
            let n: i64 = tx
                .tx()
                .query_row(
                    "SELECT COUNT(*) FROM mail_search \
                     WHERE mail_search MATCH ?1",
                    params!["stale_wrong_acct_subject_token"],
                    |r| r.get(0),
                )
                .unwrap();
            Ok(n)
        })
        .unwrap();
    assert_eq!(
        stale_hits_after, 0,
        "rebuild must wipe rows whose account_id is wrong; predicated DELETE would leave them",
    );

    // Sanity: the real envelope is still searchable.
    assert_eq!(
        count_mail_search_match(&mds, &set, &id, "real_subject_token"),
        1,
    );
    assert_eq!(
        count_mail_search_match(&mds, &set, &id, "real_body_token"),
        1,
    );
}

/// Missing-blob tolerance: when an envelope row's CAS blob is gone
/// (substrate-level corruption, partial restore, etc), rebuild must
/// still insert an envelope-only `mail_search` row and count the
/// failure in `RebuildReport.blob_load_failures`. Matches the gate-2
/// create_email policy: a missing blob doesn't lose the item from
/// header/subject search, but body search of course can't recall
/// content that isn't there. Per Codex round-1 finding on the gate-3
/// implementation — without this test the failure-count branch
/// (`blob_load_failures += 1`, empty `raw_body` substitute) is
/// untested.
#[test]
fn rebuild_search_index_indexes_envelope_when_blob_is_missing() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    let raw = b"From: alice@example.com\r\nSubject: subject_marker_token\r\nContent-Type: text/plain\r\n\r\nbody_marker_token here\r\n";
    let (hash, _) = put_blob(&mds, raw);
    let env = EmailEnvelope {
        subject: "subject_marker_token".into(),
        ..sample_envelope()
    };
    let (id, _) = store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            hash,
            env,
            &[],
            Flags(0),
            Tags::new(),
            1_700_000_000_000,
        )
        .unwrap();

    // Substitute the item row's blob_hash with a syntactically-valid
    // hex string (passes `blob::from_hex`) whose CAS file does not
    // exist. This simulates the substrate-corruption case rebuild is
    // meant to tolerate. Hex must be lowercase 64 chars to pass the
    // parser.
    let dangling_hex = "deadbeefcafebabe".repeat(4);
    debug_assert_eq!(dangling_hex.len(), 64);
    mds.with_set_tx(&set, |tx| {
        let updated = tx
            .tx()
            .execute(
                "UPDATE item SET blob_hash = ?1 WHERE id = ?2",
                params![dangling_hex, id.0.to_string()],
            )
            .map_err(|e| cosmix_mds::Error::Other(format!("update: {e}")))?;
        assert_eq!(updated, 1, "expected to update exactly one item row");
        Ok(())
    })
    .unwrap();

    // Wipe the projection so rebuild has to recreate everything from
    // scratch — drives the missing-blob path through the INSERT, not
    // a partial-update.
    wipe_mail_search(&mds, &set, TEST_ACCOUNT);
    assert_eq!(count_mail_search_rows_for_item(&mds, &set, &id), 0);

    let report = store.rebuild_search_index(TEST_ACCOUNT).unwrap();
    assert_eq!(
        report.items_rebuilt, 1,
        "envelope-only row must still be inserted",
    );
    assert_eq!(
        report.blob_load_failures, 1,
        "missing CAS blob must be counted",
    );

    // Subject and headers survived (envelope-derived columns).
    assert_eq!(count_mail_search_rows_for_item(&mds, &set, &id), 1);
    assert_eq!(
        count_mail_search_match(&mds, &set, &id, "subject_marker_token"),
        1,
        "subject column must be searchable from envelope alone",
    );
    assert_eq!(
        count_mail_search_match(&mds, &set, &id, "alice"),
        1,
        "headers column must be searchable from envelope alone",
    );
    // Body content cannot exist — the blob was unreadable.
    assert_eq!(
        count_mail_search_match(&mds, &set, &id, "body_marker_token"),
        0,
        "body column must be empty when blob load failed",
    );
}

/// Account isolation: rebuilding account A must not touch account
/// B's projection. The one-set-per-account layout makes this a
/// per-set DB operation — `account_id_to_setid` is a pure function
/// of the account id, so each account's `mail_search` lives in a
/// physically distinct SQLite file. The bare
/// `DELETE FROM mail_search` inside rebuild only touches the per-set
/// DB it ran in. This test pins that contract.
#[test]
fn rebuild_search_index_is_per_account() {
    let other_account: AccountId = 99;

    let (_d, mds, store) = open_mailstore();
    let set_a = provision(&store);
    let set_b = store.ensure_account_set(other_account).unwrap();
    let inbox_a = mk_container(&mds, &set_a, "Inbox", Some("\\Inbox"));
    let inbox_b = mk_container(&mds, &set_b, "Inbox", Some("\\Inbox"));

    let raw_a = b"From: alice@example.com\r\nSubject: account_a_token\r\nContent-Type: text/plain\r\n\r\nbody_a\r\n";
    let raw_b = b"From: bob@example.com\r\nSubject: account_b_token\r\nContent-Type: text/plain\r\n\r\nbody_b\r\n";
    let (h_a, _) = put_blob(&mds, raw_a);
    let (h_b, _) = put_blob(&mds, raw_b);
    let env_a = EmailEnvelope {
        subject: "account_a_token".into(),
        ..sample_envelope()
    };
    let env_b = EmailEnvelope {
        subject: "account_b_token".into(),
        ..sample_envelope()
    };
    let (id_a, _) = store
        .create_email(
            TEST_ACCOUNT,
            inbox_a,
            h_a,
            env_a,
            &[],
            Flags(0),
            Tags::new(),
            1_700_000_000_000,
        )
        .unwrap();
    let (id_b, _) = store
        .create_email(
            other_account,
            inbox_b,
            h_b,
            env_b,
            &[],
            Flags(0),
            Tags::new(),
            1_700_000_000_000,
        )
        .unwrap();

    // Wipe only A's projection.
    wipe_mail_search(&mds, &set_a, TEST_ACCOUNT);
    assert_eq!(
        count_mail_search_match(&mds, &set_a, &id_a, "account_a_token"),
        0
    );
    // B's projection is untouched.
    assert_eq!(
        count_mail_search_match(&mds, &set_b, &id_b, "account_b_token"),
        1
    );

    // Rebuild only A.
    let report = store.rebuild_search_index(TEST_ACCOUNT).unwrap();
    assert_eq!(report.items_rebuilt, 1);

    // A is restored.
    assert_eq!(
        count_mail_search_match(&mds, &set_a, &id_a, "account_a_token"),
        1
    );
    // B is unchanged (still 1 row matching its token).
    assert_eq!(
        count_mail_search_match(&mds, &set_b, &id_b, "account_b_token"),
        1
    );
    assert_eq!(count_mail_search_rows_for_item(&mds, &set_b, &id_b), 1);
}

/// Empty account: an account with no MDS set returns
/// `RebuildReport { 0, 0 }` without error and without bootstrapping
/// the set. Rebuild is a recovery operation, not an initialisation
/// path.
#[test]
fn rebuild_search_index_returns_zero_for_account_without_set() {
    let (_d, mds, store) = open_mailstore();
    // Deliberately do NOT call provision() — TEST_ACCOUNT has no set.
    let report = store.rebuild_search_index(TEST_ACCOUNT).unwrap();
    assert_eq!(report.items_rebuilt, 0);
    assert_eq!(report.blob_load_failures, 0);
    // The set must still be absent — rebuild didn't side-effect a
    // create_set call.
    let sets = mds.list_sets().unwrap();
    assert!(
        !sets.contains(&account_id_to_setid(TEST_ACCOUNT)),
        "rebuild should not bootstrap the MDS set: {sets:?}",
    );
}

/// Provisioned-but-empty account: the set exists but no emails have
/// been written. Rebuild reports zero items and the projection stays
/// empty.
#[test]
fn rebuild_search_index_handles_provisioned_empty_account() {
    let (_d, _mds, store) = open_mailstore();
    let _set = provision(&store);
    let report = store.rebuild_search_index(TEST_ACCOUNT).unwrap();
    assert_eq!(report.items_rebuilt, 0);
    assert_eq!(report.blob_load_failures, 0);
}

// -------------------- Phase 8e gate 4: Subject-normalization fallback --------------------

/// A reply that omits References / In-Reply-To still threads with
/// the parent when the normalized subject matches (the common
/// Outlook / mobile-MUA failure mode JMAP §4.1.5 calls out).
#[test]
fn create_email_subject_fallback_clusters_reply_without_references() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    // Parent email with subject "Topic".
    let (h_parent, _) = put_blob(&mds, b"parent");
    let parent_env = EmailEnvelope {
        from: "alice@example.com".into(),
        to: vec!["bob@example.com".into()],
        cc: Vec::new(),
        bcc: Vec::new(),
        reply_to: Vec::new(),
        subject: "Topic".into(),
        date: 1_700_000_000,
        message_id: Some("<parent@example.com>".into()),
    };
    let (_parent_id, _) = store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            h_parent,
            parent_env,
            &[],
            Flags(0),
            Tags::new(),
            1_700_000_000_000,
        )
        .unwrap();
    let parent_threads = store.list_threads(TEST_ACCOUNT, 0, 100).unwrap();
    assert_eq!(parent_threads.len(), 1);
    let parent_tid = parent_threads[0].clone();

    // Reply with subject "Re: Topic" and NO References — must still
    // thread with the parent via the normalized subject fallback.
    let (h_reply, _) = put_blob(&mds, b"reply");
    let reply_env = EmailEnvelope {
        from: "bob@example.com".into(),
        to: vec!["alice@example.com".into()],
        cc: Vec::new(),
        bcc: Vec::new(),
        reply_to: Vec::new(),
        subject: "Re: Topic".into(),
        date: 1_700_000_100,
        message_id: Some("<reply@example.com>".into()),
    };
    let (_reply_id, _) = store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            h_reply,
            reply_env,
            &[], // empty in_reply_to / references
            Flags(0),
            Tags::new(),
            1_700_000_100_000,
        )
        .unwrap();

    let threads = store.list_threads(TEST_ACCOUNT, 0, 100).unwrap();
    assert_eq!(
        threads,
        vec![parent_tid],
        "subject fallback should join the parent thread"
    );
}

/// Two emails with different normalized subjects MUST land in
/// distinct threads — the fallback must not over-cluster.
#[test]
fn create_email_subject_fallback_does_not_cluster_different_subjects() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    let (h_a, _) = put_blob(&mds, b"a");
    let env_a = EmailEnvelope {
        subject: "Topic A".into(),
        message_id: Some("<a@example.com>".into()),
        ..sample_envelope()
    };
    store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            h_a,
            env_a,
            &[],
            Flags(0),
            Tags::new(),
            1_700_000_000_000,
        )
        .unwrap();

    let (h_b, _) = put_blob(&mds, b"b");
    let env_b = EmailEnvelope {
        subject: "Topic B".into(),
        message_id: Some("<b@example.com>".into()),
        ..sample_envelope()
    };
    store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            h_b,
            env_b,
            &[],
            Flags(0),
            Tags::new(),
            1_700_000_100_000,
        )
        .unwrap();

    let threads = store.list_threads(TEST_ACCOUNT, 0, 100).unwrap();
    assert_eq!(
        threads.len(),
        2,
        "different subjects must yield distinct threads"
    );
}

/// Empty-subject emails MUST NOT cluster — empty normalized form is
/// treated as "no fallback key" so unrelated stub messages don't
/// collapse into a giant thread.
#[test]
fn create_email_subject_fallback_skips_empty_normalized_subjects() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    // Two emails with subjects that normalize to "".
    let (h_a, _) = put_blob(&mds, b"a");
    let env_a = EmailEnvelope {
        subject: "Re:".into(), // normalizes to ""
        message_id: Some("<a@example.com>".into()),
        ..sample_envelope()
    };
    store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            h_a,
            env_a,
            &[],
            Flags(0),
            Tags::new(),
            1_700_000_000_000,
        )
        .unwrap();

    let (h_b, _) = put_blob(&mds, b"b");
    let env_b = EmailEnvelope {
        subject: "".into(), // normalizes to ""
        message_id: Some("<b@example.com>".into()),
        ..sample_envelope()
    };
    store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            h_b,
            env_b,
            &[],
            Flags(0),
            Tags::new(),
            1_700_000_100_000,
        )
        .unwrap();

    let threads = store.list_threads(TEST_ACCOUNT, 0, 100).unwrap();
    assert_eq!(
        threads.len(),
        2,
        "empty-normalized subjects must yield distinct threads, not cluster"
    );
}

/// When In-Reply-To matches an existing message-id, the subject
/// fallback is irrelevant — even a wildly different subject in the
/// reply still threads with the parent via the explicit reference.
/// This pins the precedence order: References first, subject second.
#[test]
fn create_email_subject_fallback_yields_to_in_reply_to_match() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    let (h_parent, _) = put_blob(&mds, b"parent");
    let parent_env = EmailEnvelope {
        subject: "Lunch plans".into(),
        message_id: Some("<parent@example.com>".into()),
        ..sample_envelope()
    };
    let (_, _) = store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            h_parent,
            parent_env,
            &[],
            Flags(0),
            Tags::new(),
            1_700_000_000_000,
        )
        .unwrap();
    let parent_tid = store.list_threads(TEST_ACCOUNT, 0, 100).unwrap()[0].clone();

    // Reply with a totally different subject — references win.
    let (h_reply, _) = put_blob(&mds, b"reply");
    let reply_env = EmailEnvelope {
        subject: "FYI new project".into(), // normalizes differently
        message_id: Some("<reply@example.com>".into()),
        ..sample_envelope()
    };
    store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            h_reply,
            reply_env,
            &["<parent@example.com>".to_string()],
            Flags(0),
            Tags::new(),
            1_700_000_100_000,
        )
        .unwrap();

    let threads = store.list_threads(TEST_ACCOUNT, 0, 100).unwrap();
    assert_eq!(
        threads,
        vec![parent_tid],
        "in-reply-to must win over subject"
    );
}

/// `mail_envelopes.normalized_subject` is populated synchronously at
/// envelope-INSERT time with the RFC 5256 §2.1 base subject. This
/// guards against a future maintainer dropping the column-list bump
/// from the INSERT shape.
#[test]
fn create_email_writes_normalized_subject_column() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    let (hash, _) = put_blob(&mds, b"normalize me");
    let env = EmailEnvelope {
        subject: "[list] Re: Hello (fwd)".into(),
        message_id: Some("<n@example.com>".into()),
        ..sample_envelope()
    };
    let (item_id, _) = store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            hash,
            env,
            &[],
            Flags(0),
            Tags::new(),
            1_700_000_000_000,
        )
        .unwrap();

    let stored: String = mds
        .with_set_tx(&set, |tx| {
            let s: String = tx
                .tx()
                .query_row(
                    "SELECT normalized_subject FROM mail_envelopes WHERE item_id = ?1",
                    params![item_id.0.to_string()],
                    |r| r.get(0),
                )
                .unwrap();
            Ok::<_, cosmix_mds::Error>(s)
        })
        .unwrap();
    assert_eq!(stored, "hello", "RFC 5256 §2.1 base subject must be stored");
}

/// Subject fallback is per-account: an account-A email with no
/// References whose subject matches an account-B email MUST NOT join
/// account B's thread. The `WHERE account_id = ?` clause in the
/// fallback lookup enforces the same per-account isolation the
/// References lookup has.
#[test]
fn create_email_subject_fallback_is_per_account() {
    let (_d, mds, store) = open_mailstore();
    let set_a = provision(&store);
    let inbox_a = mk_container(&mds, &set_a, "Inbox", Some("\\Inbox"));

    let other_account: AccountId = TEST_ACCOUNT + 1;
    let set_b = store.ensure_account_set(other_account).unwrap();
    let inbox_b = mk_container(&mds, &set_b, "Inbox", Some("\\Inbox"));

    let (h_b, _) = put_blob(&mds, b"in-b");
    let env_b = EmailEnvelope {
        subject: "Shared subject".into(),
        message_id: Some("<b@example.com>".into()),
        ..sample_envelope()
    };
    store
        .create_email(
            other_account,
            inbox_b,
            h_b,
            env_b,
            &[],
            Flags(0),
            Tags::new(),
            1_700_000_000_000,
        )
        .unwrap();

    let (h_a, _) = put_blob(&mds, b"in-a");
    let env_a = EmailEnvelope {
        subject: "Shared subject".into(),
        message_id: Some("<a@example.com>".into()),
        ..sample_envelope()
    };
    store
        .create_email(
            TEST_ACCOUNT,
            inbox_a,
            h_a,
            env_a,
            &[],
            Flags(0),
            Tags::new(),
            1_700_000_100_000,
        )
        .unwrap();

    let threads_a = store.list_threads(TEST_ACCOUNT, 0, 100).unwrap();
    let threads_b = store.list_threads(other_account, 0, 100).unwrap();
    assert_eq!(threads_a.len(), 1);
    assert_eq!(threads_b.len(), 1);
    assert_ne!(
        threads_a[0].0, threads_b[0].0,
        "subject fallback must not cross account boundaries"
    );
}

/// When several existing threads share the same normalized subject —
/// e.g. unrelated "weekly status" threads months apart — a new
/// reference-less reply must deterministically join the MOST RECENT
/// matching thread. The lookup ORDER BY pins this; without it
/// SQLite's free choice of plan would make the selected thread
/// arbitrary and flap on page-order changes.
#[test]
fn create_email_subject_fallback_picks_most_recent_when_multiple_match() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    // Older thread root.
    let (h_old, _) = put_blob(&mds, b"old");
    let old_env = EmailEnvelope {
        from: "alice@example.com".into(),
        to: vec!["bob@example.com".into()],
        cc: Vec::new(),
        bcc: Vec::new(),
        reply_to: Vec::new(),
        subject: "Weekly status".into(),
        date: 1_600_000_000,
        message_id: Some("<old@example.com>".into()),
    };
    let (old_item_id, _) = store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            h_old,
            old_env,
            &[],
            Flags(0),
            Tags::new(),
            1_600_000_000_000,
        )
        .unwrap();

    // Newer email, same normalized subject — under the production
    // fallback this clusters with the older one via Step 2. Stand
    // up TWO distinct threads sharing the same subject by directly
    // rewriting the newer item's mail_threads row to a fresh
    // thread_id below. This models the real-world case where two
    // unrelated topics happen to share a subject and have already
    // been threaded apart (e.g. via reference-aware import).
    let (h_new, _) = put_blob(&mds, b"new");
    let new_env = EmailEnvelope {
        from: "alice@example.com".into(),
        to: vec!["bob@example.com".into()],
        cc: Vec::new(),
        bcc: Vec::new(),
        reply_to: Vec::new(),
        subject: "Weekly status".into(),
        date: 1_700_000_000,
        message_id: Some("<new@example.com>".into()),
    };
    let (new_item_id, _) = store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            h_new,
            new_env,
            &[],
            Flags(0),
            Tags::new(),
            1_700_000_000_000,
        )
        .unwrap();

    // Sanity-check: subject fallback joined both into one thread.
    fn thread_id_of(
        mds: &cosmix_mds::SqliteCasMds,
        set: &cosmix_mds::SetId,
        account: AccountId,
        item: ItemId,
    ) -> String {
        let mut out = String::new();
        mds.with_set_tx(set, |tx| {
            out = tx
                .tx()
                .query_row(
                    "SELECT thread_id FROM mail_threads \
                     WHERE item_id = ?1 AND account_id = ?2",
                    rusqlite::params![item.0.to_string(), account],
                    |r| r.get::<_, String>(0),
                )
                .map_err(|e| cosmix_mds::Error::Other(format!("thread_id_of: {e}")))?;
            Ok(())
        })
        .unwrap();
        out
    }
    let old_thread_initial = thread_id_of(&mds, &set, TEST_ACCOUNT, old_item_id);
    let new_thread_initial = thread_id_of(&mds, &set, TEST_ACCOUNT, new_item_id);
    assert_eq!(
        old_thread_initial, new_thread_initial,
        "precondition: subject fallback joined both into one thread"
    );

    // Repoint the *newer* item to a fresh thread_id so we end up
    // with two distinct threads sharing the same normalized subject.
    let new_thread_id = uuid::Uuid::new_v4().to_string();
    {
        let new_thread_id = new_thread_id.clone();
        mds.with_set_tx(&set, |tx| {
            tx.tx()
                .execute(
                    "UPDATE mail_threads SET thread_id = ?1 \
                     WHERE item_id = ?2 AND account_id = ?3",
                    rusqlite::params![new_thread_id, new_item_id.0.to_string(), TEST_ACCOUNT],
                )
                .map_err(|e| cosmix_mds::Error::Other(format!("seed second thread: {e}")))?;
            Ok(())
        })
        .unwrap();
    }

    // Now drop a reference-less reply. The fallback MUST pick the
    // newer thread (date_ts=1_700_000_000 > 1_600_000_000), not the
    // older one.
    let (h_reply, _) = put_blob(&mds, b"reply");
    let reply_env = EmailEnvelope {
        from: "bob@example.com".into(),
        to: vec!["alice@example.com".into()],
        cc: Vec::new(),
        bcc: Vec::new(),
        reply_to: Vec::new(),
        subject: "Re: Weekly status".into(),
        date: 1_700_000_100,
        message_id: Some("<reply@example.com>".into()),
    };
    let (reply_item_id, _) = store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            h_reply,
            reply_env,
            &[],
            Flags(0),
            Tags::new(),
            1_700_000_100_000,
        )
        .unwrap();

    let reply_thread = thread_id_of(&mds, &set, TEST_ACCOUNT, reply_item_id);
    assert_eq!(
        reply_thread, new_thread_id,
        "subject fallback must pick the most recent matching thread"
    );
    // Older thread is unchanged.
    let old_thread_now = thread_id_of(&mds, &set, TEST_ACCOUNT, old_item_id);
    assert_eq!(
        old_thread_now, old_thread_initial,
        "older thread must remain untouched"
    );
}

/// Tie-break determinism when two threads share the same normalized
/// subject AND the same `date_ts`, with NULL `message_id` on both.
/// The schema makes `message_id` nullable and there is no DB-level
/// uniqueness constraint, so the fallback ORDER BY MUST fall through
/// to `item_id DESC` as a total-order tie-breaker; otherwise the
/// selected thread is arbitrary and would flap on page-order changes.
#[test]
fn create_email_subject_fallback_tie_break_uses_item_id_when_message_id_null() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    // Two roots with identical (subject, date_ts) and NULL
    // message_id — the worst-case tie-break input.
    let (h_a, _) = put_blob(&mds, b"a");
    let env_a = EmailEnvelope {
        from: "alice@example.com".into(),
        to: vec!["bob@example.com".into()],
        cc: Vec::new(),
        bcc: Vec::new(),
        reply_to: Vec::new(),
        subject: "Outage post-mortem".into(),
        date: 1_700_000_000,
        message_id: None,
    };
    let (item_a, _) = store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            h_a,
            env_a,
            &[],
            Flags(0),
            Tags::new(),
            1_700_000_000_000,
        )
        .unwrap();

    let (h_b, _) = put_blob(&mds, b"b");
    let env_b = EmailEnvelope {
        from: "alice@example.com".into(),
        to: vec!["bob@example.com".into()],
        cc: Vec::new(),
        bcc: Vec::new(),
        reply_to: Vec::new(),
        subject: "Outage post-mortem".into(),
        date: 1_700_000_000,
        message_id: None,
    };
    let (item_b, _) = store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            h_b,
            env_b,
            &[],
            Flags(0),
            Tags::new(),
            1_700_000_001_000,
        )
        .unwrap();

    // Force two distinct threads via direct mail_threads UPDATE on
    // the second item. item_id is a UUIDv4 TEXT column compared
    // lexically (NOT time-ordered), so the tie-break selects whichever
    // value sorts greater under SQLite TEXT collation — that's the
    // invariant this test pins.
    let thread_b = uuid::Uuid::new_v4().to_string();
    {
        let thread_b = thread_b.clone();
        mds.with_set_tx(&set, |tx| {
            tx.tx()
                .execute(
                    "UPDATE mail_threads SET thread_id = ?1 \
                     WHERE item_id = ?2 AND account_id = ?3",
                    rusqlite::params![thread_b, item_b.0.to_string(), TEST_ACCOUNT],
                )
                .map_err(|e| cosmix_mds::Error::Other(format!("seed thread_b: {e}")))?;
            Ok(())
        })
        .unwrap();
    }

    // Determine which item_id is greater — the ORDER BY ... item_id
    // DESC LIMIT 1 must pick the row owning that one.
    let a_str = item_a.0.to_string();
    let b_str = item_b.0.to_string();
    let (winner_thread_expected, winner_label) = if b_str > a_str {
        (thread_b.clone(), "item_b (greater item_id)")
    } else {
        // item_a still on the original thread (subject-fallback
        // joined them before the UPDATE, so item_a's thread_id is
        // unchanged from the initial value).
        let original = {
            let mut out = String::new();
            mds.with_set_tx(&set, |tx| {
                out = tx
                    .tx()
                    .query_row(
                        "SELECT thread_id FROM mail_threads \
                         WHERE item_id = ?1 AND account_id = ?2",
                        rusqlite::params![a_str.clone(), TEST_ACCOUNT],
                        |r| r.get::<_, String>(0),
                    )
                    .map_err(|e| cosmix_mds::Error::Other(format!("read thread_a: {e}")))?;
                Ok(())
            })
            .unwrap();
            out
        };
        (original, "item_a (greater item_id)")
    };

    // Drop a reference-less reply with same date_ts and NULL
    // message_id — leans entirely on the tie-breaker.
    let (h_reply, _) = put_blob(&mds, b"reply");
    let reply_env = EmailEnvelope {
        from: "bob@example.com".into(),
        to: vec!["alice@example.com".into()],
        cc: Vec::new(),
        bcc: Vec::new(),
        reply_to: Vec::new(),
        subject: "Re: Outage post-mortem".into(),
        date: 1_700_000_000,
        message_id: None,
    };
    let (reply_item, _) = store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            h_reply,
            reply_env,
            &[],
            Flags(0),
            Tags::new(),
            1_700_000_002_000,
        )
        .unwrap();

    let reply_thread = {
        let mut out = String::new();
        mds.with_set_tx(&set, |tx| {
            out = tx
                .tx()
                .query_row(
                    "SELECT thread_id FROM mail_threads \
                     WHERE item_id = ?1 AND account_id = ?2",
                    rusqlite::params![reply_item.0.to_string(), TEST_ACCOUNT],
                    |r| r.get::<_, String>(0),
                )
                .map_err(|e| cosmix_mds::Error::Other(format!("read reply thread: {e}")))?;
            Ok(())
        })
        .unwrap();
        out
    };
    assert_eq!(
        reply_thread, winner_thread_expected,
        "tie-break must follow item_id DESC ({winner_label} wins)"
    );
}

// -------------------- Task 3.0: query_emails --------------------

/// `inMailbox: <X>` returns only items with membership in X, and the
/// `before` / `after` window narrows the result to a half-open range
/// on `received_at` (`>= after`, `< before`). Ordering follows
/// `EmailSort::ReceivedAtDesc`.
#[test]
fn query_emails_with_mailbox_and_time_window() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));
    let archive = mk_container(&mds, &set, "Archive", None);

    // Three items in Inbox with strictly-ordered received_at; one
    // item in Archive that the filter must exclude.
    let id_a = add_item_in(&mds, &set, inbox, b"a");
    let id_b = add_item_in(&mds, &set, inbox, b"b");
    let id_c = add_item_in(&mds, &set, inbox, b"c");
    let id_archive = add_item_in(&mds, &set, archive, b"d");

    set_received_at(&mds, &set, &id_a, 1_000);
    set_received_at(&mds, &set, &id_b, 2_000);
    set_received_at(&mds, &set, &id_c, 3_000);
    set_received_at(&mds, &set, &id_archive, 2_000);

    // Window [1500, 3000): should return id_b only (id_a is below
    // `after`, id_c is at `before` which is exclusive, id_archive is
    // in a different mailbox).
    let filter = EmailQueryFilter {
        in_mailbox: Some(inbox),
        after: Some(1_500),
        before: Some(3_000),
        ..EmailQueryFilter::default()
    };
    let out = store
        .query_emails(
            TEST_ACCOUNT,
            filter,
            EmailSort::ReceivedAtDesc,
            ListOpts::default(),
        )
        .unwrap();
    assert_eq!(out, vec![id_b]);

    // Drop the `before` cap → id_c joins the result. Desc order
    // means id_c (3000) comes before id_b (2000).
    let filter = EmailQueryFilter {
        in_mailbox: Some(inbox),
        after: Some(1_500),
        ..EmailQueryFilter::default()
    };
    let out = store
        .query_emails(
            TEST_ACCOUNT,
            filter,
            EmailSort::ReceivedAtDesc,
            ListOpts::default(),
        )
        .unwrap();
    assert_eq!(out, vec![id_c, id_b]);

    // Asc order flips it.
    let filter = EmailQueryFilter {
        in_mailbox: Some(inbox),
        after: Some(1_500),
        ..EmailQueryFilter::default()
    };
    let out = store
        .query_emails(
            TEST_ACCOUNT,
            filter,
            EmailSort::ReceivedAtAsc,
            ListOpts::default(),
        )
        .unwrap();
    assert_eq!(out, vec![id_b, id_c]);
}

/// Phase B sort: `subject` orders by the RFC base subject (prefix-
/// stripped + lowercased, so "Re: Banana" keys as "banana"); `from`
/// orders by the lowercased From address. Both directions reverse
/// cleanly. Keys come from the `mail_envelopes` sidecar.
#[test]
fn query_emails_sorts_by_subject_and_from() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    let id_a = add_item_in(&mds, &set, inbox, b"a");
    let id_b = add_item_in(&mds, &set, inbox, b"b");
    let id_c = add_item_in(&mds, &set, inbox, b"c");
    set_received_at(&mds, &set, &id_a, 3_000);
    set_received_at(&mds, &set, &id_b, 2_000);
    set_received_at(&mds, &set, &id_c, 1_000);

    let env = |from: &str, subject: &str| EmailEnvelope {
        from: from.into(),
        to: Vec::new(),
        cc: Vec::new(),
        bcc: Vec::new(),
        reply_to: Vec::new(),
        subject: subject.into(),
        date: 0,
        message_id: None,
    };
    // subject keys: a->"banana" (Re: stripped + lowercased), b->"apple", c->"cherry".
    // from keys:    a->"zoe…", b->"mike…", c->"anna…".
    insert_envelope(&mds, &set, &id_a, &env("zoe@example.com", "Re: Banana"));
    insert_envelope(&mds, &set, &id_b, &env("mike@example.com", "apple"));
    insert_envelope(&mds, &set, &id_c, &env("anna@example.com", "Cherry"));

    let q = |sort| {
        store
            .query_emails(
                TEST_ACCOUNT,
                EmailQueryFilter {
                    in_mailbox: Some(inbox),
                    ..EmailQueryFilter::default()
                },
                sort,
                ListOpts::default(),
            )
            .unwrap()
    };

    assert_eq!(
        q(EmailSort::SubjectAsc),
        vec![id_b, id_a, id_c],
        "subject asc"
    );
    assert_eq!(
        q(EmailSort::SubjectDesc),
        vec![id_c, id_a, id_b],
        "subject desc"
    );
    assert_eq!(q(EmailSort::FromAsc), vec![id_c, id_b, id_a], "from asc");
    assert_eq!(q(EmailSort::FromDesc), vec![id_a, id_b, id_c], "from desc");
}

/// `has_keyword` filters on the **union** of `Flags` across an
/// item's memberships (RFC 8621 §4.1.4 union semantics). An item
/// with `\Seen` set in any one of its memberships passes
/// `has_keyword: \Seen`; an item with the bit cleared everywhere
/// fails. `not_keyword` is the inverse.
#[test]
fn query_emails_has_keyword_uses_union_across_memberships() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));
    let archive = mk_container(&mds, &set, "Archive", None);

    // id_seen: in Inbox with \Seen set (bit 0). id_unread: in
    // Inbox with no flags. id_seen_via_archive: present in both
    // Inbox (no flags) and Archive (\Seen set) — must surface as
    // "has \Seen" because keywords are union across memberships.
    let id_seen = add_item_in(&mds, &set, inbox, b"seen");
    let id_unread = add_item_in(&mds, &set, inbox, b"unread");
    let id_seen_via_archive = add_item_in(&mds, &set, inbox, b"crossover");

    // Flip \Seen on the in-Inbox membership for id_seen, and on the
    // (about-to-be-added) in-Archive membership for id_seen_via_archive.
    store
        .set_keywords(TEST_ACCOUNT, id_seen, Flags(0b0001), Tags::new())
        .unwrap();
    // id_seen_via_archive must have a separate Archive membership
    // carrying \Seen while its Inbox membership stays clear, so we
    // use add_item again to attach Archive then set keywords from a
    // direct UPDATE on that one membership.
    mds.with_set_tx(&set, |tx| {
        // Add an Archive membership for id_seen_via_archive with
        // flags=1 (\Seen). The cosmix-mds public API only exposes
        // add_item for the initial create, so use the direct INSERT.
        tx.tx()
            .execute(
                "INSERT INTO membership (item_id, container_id, flags, tags, seq, change_seq, added_at) \
                 VALUES (?1, ?2, 1, '[]', 1, 1, 0)",
                params![id_seen_via_archive.0.to_string(), archive.0.to_string()],
            )
            .unwrap();
        Ok(())
    })
    .unwrap();

    // has_keyword: \Seen → matches id_seen and id_seen_via_archive,
    // skips id_unread.
    let filter = EmailQueryFilter {
        in_mailbox: Some(inbox),
        has_keyword: Some(Flags(0b0001)),
        ..EmailQueryFilter::default()
    };
    let mut out = store
        .query_emails(
            TEST_ACCOUNT,
            filter,
            EmailSort::ReceivedAtAsc,
            ListOpts::default(),
        )
        .unwrap();
    out.sort_by_key(|i| i.0.to_string());
    let mut want = vec![id_seen, id_seen_via_archive];
    want.sort_by_key(|i| i.0.to_string());
    assert_eq!(out, want);

    // not_keyword: \Seen → matches id_unread only.
    let filter = EmailQueryFilter {
        in_mailbox: Some(inbox),
        not_keyword: Some(Flags(0b0001)),
        ..EmailQueryFilter::default()
    };
    let out = store
        .query_emails(
            TEST_ACCOUNT,
            filter,
            EmailSort::ReceivedAtAsc,
            ListOpts::default(),
        )
        .unwrap();
    assert_eq!(out, vec![id_unread]);
}

/// `in_mailbox: None` walks every container in the account and
/// dedupes items that live in more than one mailbox. The
/// `__upload_staging__` container is excluded so a blob held there
/// does not surface as an Email.
#[test]
fn query_emails_account_wide_dedupes_and_excludes_staging() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));
    let archive = mk_container(&mds, &set, "Archive", None);

    // id_inbox lives only in Inbox. id_both lives in Inbox AND
    // Archive — must surface exactly once. id_archive lives only
    // in Archive.
    let id_inbox = add_item_in(&mds, &set, inbox, b"only-inbox");
    let id_archive = add_item_in(&mds, &set, archive, b"only-archive");

    let (hash, _) = put_blob(&mds, b"both");
    let id_both = mds
        .add_item(
            &set,
            &hash,
            &[
                Membership {
                    container: inbox,
                    flags: Flags(0),
                    added_at: 0,
                },
                Membership {
                    container: archive,
                    flags: Flags(0),
                    added_at: 0,
                },
            ],
        )
        .unwrap()
        .item_id;

    // Drop a blob into __upload_staging__ via create_blob_ref. The
    // Email/query result MUST NOT include this synthetic item.
    let staging_bytes = b"upload-staging-payload";
    let (staging_hash, staging_size) = put_blob(&mds, staging_bytes);
    store
        .create_blob_ref(TEST_ACCOUNT, staging_hash, staging_size, 9_999_999_999)
        .unwrap();

    set_received_at(&mds, &set, &id_inbox, 1_000);
    set_received_at(&mds, &set, &id_archive, 2_000);
    set_received_at(&mds, &set, &id_both, 3_000);

    let out = store
        .query_emails(
            TEST_ACCOUNT,
            EmailQueryFilter::default(),
            EmailSort::ReceivedAtDesc,
            ListOpts::default(),
        )
        .unwrap();
    // Three distinct items, descending received_at. id_both surfaces
    // exactly once despite living in two containers; the staging
    // blob is filtered out.
    assert_eq!(out, vec![id_both, id_archive, id_inbox]);
}

/// `ListOpts::offset` and `ListOpts::limit` slice the sorted result.
/// `offset >= len` returns empty; `offset + limit` overrunning `len`
/// is clamped to the tail.
#[test]
fn query_emails_paginates_with_offset_and_limit() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    let mut ids = Vec::new();
    for i in 0..5u32 {
        let id = add_item_in(&mds, &set, inbox, format!("p{i}").as_bytes());
        set_received_at(&mds, &set, &id, 1_000 + i64::from(i));
        ids.push(id);
    }
    // Asc order matches insertion.
    let asc = ids.clone();

    // Page 1: first two.
    let opts = ListOpts {
        limit: 2,
        offset: 0,
        sort_by: SortKey::default(),
    };
    let out = store
        .query_emails(
            TEST_ACCOUNT,
            EmailQueryFilter::default(),
            EmailSort::ReceivedAtAsc,
            opts,
        )
        .unwrap();
    assert_eq!(out, asc[0..2].to_vec());

    // Page 2: skip 2, take 2.
    let opts = ListOpts {
        limit: 2,
        offset: 2,
        sort_by: SortKey::default(),
    };
    let out = store
        .query_emails(
            TEST_ACCOUNT,
            EmailQueryFilter::default(),
            EmailSort::ReceivedAtAsc,
            opts,
        )
        .unwrap();
    assert_eq!(out, asc[2..4].to_vec());

    // Page 3: limit overruns end → tail clamp to one element.
    let opts = ListOpts {
        limit: 2,
        offset: 4,
        sort_by: SortKey::default(),
    };
    let out = store
        .query_emails(
            TEST_ACCOUNT,
            EmailQueryFilter::default(),
            EmailSort::ReceivedAtAsc,
            opts,
        )
        .unwrap();
    assert_eq!(out, asc[4..5].to_vec());

    // offset >= len → empty.
    let opts = ListOpts {
        limit: 10,
        offset: 5,
        sort_by: SortKey::default(),
    };
    let out = store
        .query_emails(
            TEST_ACCOUNT,
            EmailQueryFilter::default(),
            EmailSort::ReceivedAtAsc,
            opts,
        )
        .unwrap();
    assert!(out.is_empty());
}

/// `text` full-text filter: matches the `mail_search` FTS5 projection
/// (subject + folded headers + normalized addrs + plain-text body), ANDs
/// with `in_mailbox`, and yields nothing for a non-matching / blank needle.
#[test]
fn query_emails_text_filter_matches_subject_addr_and_body() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    // e1: distinctive SUBJECT token; e2: distinctive FROM address; e3: a
    // valid RFC 5322 blob so the plain-text BODY projects into mail_search.
    // (sample_envelope's `to` is shared, so the distinguishing terms are the
    // subject / from / body — never the shared `to` addresses.)
    let (h1, _) = put_blob(&mds, b"body one");
    let mut e1 = sample_envelope();
    e1.subject = "Quarterly invoice attached".into();
    e1.from = "zara@example.com".into();
    let (id1, _) = store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            h1,
            e1,
            &[],
            Flags(0),
            Tags::new(),
            1_700_000_000_000,
        )
        .unwrap();

    let (h2, _) = put_blob(&mds, b"body two");
    let mut e2 = sample_envelope();
    e2.subject = "Lunch plans".into();
    e2.from = "yorick@example.com".into();
    let (id2, _) = store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            h2,
            e2,
            &[],
            Flags(0),
            Tags::new(),
            1_700_000_001_000,
        )
        .unwrap();

    let (h3, _) = put_blob(
        &mds,
        b"From: wendy@example.com\r\nSubject: s\r\nContent-Type: text/plain\r\n\r\nthe magic widget word\r\n",
    );
    let mut e3 = sample_envelope();
    e3.subject = "plain subject".into();
    e3.from = "wendy@example.com".into();
    let (id3, _) = store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            h3,
            e3,
            &[],
            Flags(0),
            Tags::new(),
            1_700_000_002_000,
        )
        .unwrap();

    let run = |needle: &str| {
        let f = EmailQueryFilter {
            in_mailbox: Some(inbox),
            text: Some(needle.to_string()),
            ..Default::default()
        };
        store
            .query_emails(
                TEST_ACCOUNT,
                f,
                EmailSort::ReceivedAtDesc,
                ListOpts::default(),
            )
            .unwrap()
    };

    // Subject token → only e1.
    assert_eq!(run("invoice"), vec![id1]);
    // From-address token (normalized_addrs) → only e2.
    assert_eq!(run("yorick"), vec![id2]);
    // Plain-text BODY token (projected from the blob) → only e3.
    assert_eq!(run("widget"), vec![id3]);
    // No match → empty.
    assert!(run("zzznomatchterm").is_empty());
    // Blank / punctuation-only needle matches NOTHING, not everything.
    assert!(run("   ").is_empty());
    assert!(run("- ^ ( )").is_empty());
}

/// `in_mailbox_other_than: [X]` returns items with at least one
/// membership outside the listed set. An item whose only memberships
/// are inside the listed set fails. (RFC 8621 §4.4.1.)
#[test]
fn query_emails_in_mailbox_other_than_semantics() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));
    let archive = mk_container(&mds, &set, "Archive", None);
    let trash = mk_container(&mds, &set, "Trash", None);

    // id_inbox_only: only in Inbox → fails (no mailbox other than Inbox).
    let id_inbox_only = add_item_in(&mds, &set, inbox, b"inbox-only");

    // id_inbox_archive: in Inbox + Archive → passes (Archive is "other").
    let (h2, _) = put_blob(&mds, b"inbox-archive");
    let id_inbox_archive = mds
        .add_item(
            &set,
            &h2,
            &[
                Membership {
                    container: inbox,
                    flags: Flags(0),
                    added_at: 0,
                },
                Membership {
                    container: archive,
                    flags: Flags(0),
                    added_at: 0,
                },
            ],
        )
        .unwrap()
        .item_id;

    // id_trash: only in Trash → passes (Trash is "other").
    let id_trash = add_item_in(&mds, &set, trash, b"trash-only");

    set_received_at(&mds, &set, &id_inbox_only, 1_000);
    set_received_at(&mds, &set, &id_inbox_archive, 2_000);
    set_received_at(&mds, &set, &id_trash, 3_000);

    let filter = EmailQueryFilter {
        in_mailbox_other_than: Some(vec![inbox]),
        ..EmailQueryFilter::default()
    };
    let out = store
        .query_emails(
            TEST_ACCOUNT,
            filter,
            EmailSort::ReceivedAtDesc,
            ListOpts::default(),
        )
        .unwrap();
    assert_eq!(out, vec![id_trash, id_inbox_archive]);
}

// ---------------------------------------------------------------------
// IMAP-readiness contract stubs.
//
// These tests describe substrate properties the IMAP module
// (`cosmix-maild::imap`, see `_doc/maild/imap.md` R2) requires from
// `MailStore`. They are **#[ignore]'d panics** today, matching the shape
// of the JMAP `email_set_contract_*` stubs in `jmap/email.rs::tests`.
//
// Each one is bound to a prerequisite in
// `_doc/planned/imap-runway.md` (P1 / P5 / P6 / P7). When the
// corresponding trait extension lands, the test body is replaced with
// a real assertion against `SqliteMailStore` and the `#[ignore]` is
// removed.
//
// IMAP non-negotiables involved here are listed in `_doc/maild/imap.md`
// §Non-negotiable invariants (1, 2, 4, 5, 6, 8).
// ---------------------------------------------------------------------

/// P1 (`imap-runway.md`) — per-mailbox flag isolation. IMAP
/// non-negotiable invariant 5/6: flags are per `(message,
/// mailbox)`; `\Seen` in INBOX must not propagate to Archive.
///
/// `MailStore::set_keywords` (the JMAP-shaped path) applies the
/// keyword set across **every** mailbox the email belongs to —
/// the IMAP-shaped sibling
/// `MailStore::set_keywords_in_mailbox(account, item, mailbox,
/// flags)` must mutate exactly one membership row.
#[test]
fn imap_contract_per_membership_flag_isolation() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));
    let archive = mk_container(&mds, &set, "Archive", Some("\\Archive"));

    // One item in both mailboxes, both memberships start with flags=0.
    let (hash, _) = put_blob(&mds, b"per-membership isolation");
    let memberships = [
        Membership {
            container: inbox,
            flags: Flags(0),
            added_at: 0,
        },
        Membership {
            container: archive,
            flags: Flags(0),
            added_at: 0,
        },
    ];
    let id = mds.add_item(&set, &hash, &memberships).unwrap().item_id;

    // Set `\Seen` (bit 0) on the INBOX membership only.
    store
        .set_keywords_in_mailbox(TEST_ACCOUNT, id, inbox, Flags(0b0001), Tags::new())
        .unwrap();

    let rows = mds.item_memberships(&set, &id).unwrap();
    let inbox_flags = rows.iter().find(|(c, _, _)| *c == inbox).unwrap().1;
    let archive_flags = rows.iter().find(|(c, _, _)| *c == archive).unwrap().1;
    assert_eq!(
        inbox_flags,
        Flags(0b0001),
        "INBOX membership must carry \\Seen"
    );
    assert_eq!(
        archive_flags,
        Flags(0),
        "Archive membership must remain untouched (the load-bearing IMAP invariant)"
    );

    // The JMAP-shaped sibling fans out — exercise it on the same item
    // to confirm the two paths are not collapsed.
    store
        .set_keywords(TEST_ACCOUNT, id, Flags(0b0010), Tags::new())
        .unwrap();
    let rows = mds.item_memberships(&set, &id).unwrap();
    for (_, flags, _) in &rows {
        assert_eq!(
            *flags,
            Flags(0b0010),
            "JMAP set_keywords applies to every membership"
        );
    }

    // STORE against a (item, mailbox) pair with no membership row in
    // an *existing* mailbox surfaces "email not found" — IMAP UID
    // resolution-race contract.
    let bogus_mailbox = mk_container(&mds, &set, "Drafts", Some("\\Drafts"));
    let err = store
        .set_keywords_in_mailbox(TEST_ACCOUNT, id, bogus_mailbox, Flags(0b0001), Tags::new())
        .unwrap_err();
    let msg = format!("{err}");
    assert!(msg.starts_with("email not found"), "got: {msg}");

    // STORE against a mailbox that *doesn't exist at all* (caller
    // typo, or deletion between SELECT and STORE) surfaces "mailbox
    // not found", not "email not found". The store_flags_in_tx
    // primitive disambiguates ContainerNotFound from
    // membership-missing so the wrapper can keep the wire shapes
    // separate.
    let unknown_mailbox = ContainerId(uuid::Uuid::new_v4());
    let err = store
        .set_keywords_in_mailbox(
            TEST_ACCOUNT,
            id,
            unknown_mailbox,
            Flags(0b0001),
            Tags::new(),
        )
        .unwrap_err();
    let msg = format!("{err}");
    assert!(msg.starts_with("mailbox not found"), "got: {msg}");

    // Unprovisioned account surfaces "mailbox not found".
    let err = store
        .set_keywords_in_mailbox(TEST_ACCOUNT + 99, id, inbox, Flags(0b0001), Tags::new())
        .unwrap_err();
    let msg = format!("{err}");
    assert!(msg.starts_with("mailbox not found"), "got: {msg}");
}

/// W1 — user-keyword (`Tags`) facet through the `MailStore` trait.
/// Proves the `crate::keyword` and MailStore Tags contract: the
/// substrate-read + keyword-write + query surface carries `Tags`
/// alongside `Flags`, obeying the same per-mailbox-vs-whole-item split
/// the system flags already do:
///   * `set_keywords_in_mailbox` writes tags on ONE membership only
///     (the IMAP STORE isolation invariant, now for user keywords);
///   * `get_email` unions tags across memberships (JMAP `keywords`);
///   * `query_emails` `has_tag` / `not_tag` filter on that union;
///   * `set_keywords` replaces flags + tags on EVERY membership (JMAP
///     `Email/set keywords` is the whole-item set, not a delta).
#[test]
fn user_keywords_tags_facet_round_trip() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));
    let archive = mk_container(&mds, &set, "Archive", Some("\\Archive"));

    // One item in both mailboxes; both memberships start flags=0,
    // tags={}. Use create_email_with_memberships (not raw mds.add_item)
    // so the `mail_envelopes` sidecar exists and get_email resolves.
    let (hash, _size) = put_blob(&mds, b"tags facet");
    let (id, _uids) = store
        .create_email_with_memberships(
            TEST_ACCOUNT,
            &[inbox, archive],
            hash,
            sample_envelope(),
            &[],
            Flags(0),
            Tags::new(),
            1_730_000_000_000,
        )
        .unwrap();

    // (1) Per-mailbox write: `\Seen` + "public" on the INBOX membership
    // only — tags obey the same per-membership isolation as flags.
    store
        .set_keywords_in_mailbox(
            TEST_ACCOUNT,
            id,
            inbox,
            Flags(0b0001),
            Tags::from(vec!["public".to_string()]),
        )
        .unwrap();
    let rows = mds.item_memberships(&set, &id).unwrap();
    let inbox_row = rows.iter().find(|(c, _, _)| *c == inbox).unwrap();
    let archive_row = rows.iter().find(|(c, _, _)| *c == archive).unwrap();
    assert_eq!(inbox_row.1, Flags(0b0001));
    assert_eq!(inbox_row.2, Tags::from(vec!["public".to_string()]));
    assert_eq!(archive_row.1, Flags(0));
    assert!(
        archive_row.2.is_empty(),
        "Archive membership tags must stay empty (per-membership isolation)"
    );

    // (2) get_email unions tags across memberships, exactly as keywords.
    let rec = store.get_email(TEST_ACCOUNT, id).unwrap();
    assert_eq!(rec.keywords, Flags(0b0001));
    assert_eq!(rec.tags, Tags::from(vec!["public".to_string()]));

    // (3) query_emails has_tag / not_tag filter on the tag union.
    let q = |f: EmailQueryFilter| {
        store
            .query_emails(
                TEST_ACCOUNT,
                f,
                EmailSort::ReceivedAtAsc,
                ListOpts::default(),
            )
            .unwrap()
    };
    assert_eq!(
        q(EmailQueryFilter {
            in_mailbox: Some(inbox),
            has_tag: Some("public".to_string()),
            ..EmailQueryFilter::default()
        }),
        vec![id],
        "has_tag matches the tagged item"
    );
    assert!(
        q(EmailQueryFilter {
            in_mailbox: Some(inbox),
            has_tag: Some("private".to_string()),
            ..EmailQueryFilter::default()
        })
        .is_empty(),
        "has_tag for an absent keyword matches nothing"
    );
    assert!(
        q(EmailQueryFilter {
            in_mailbox: Some(inbox),
            not_tag: Some("public".to_string()),
            ..EmailQueryFilter::default()
        })
        .is_empty(),
        "not_tag excludes the item carrying the keyword"
    );
    assert_eq!(
        q(EmailQueryFilter {
            in_mailbox: Some(inbox),
            not_tag: Some("private".to_string()),
            ..EmailQueryFilter::default()
        }),
        vec![id],
        "not_tag for an absent keyword keeps the item"
    );

    // (4) Whole-item write replaces flags + tags on EVERY membership —
    // the prior `\Seen` + "public" are gone (JMAP keywords is the
    // complete set, not a delta).
    store
        .set_keywords(
            TEST_ACCOUNT,
            id,
            Flags(0b0010),
            Tags::from(vec!["draft".to_string(), "pinned".to_string()]),
        )
        .unwrap();
    let rows = mds.item_memberships(&set, &id).unwrap();
    let want = Tags::from(vec!["draft".to_string(), "pinned".to_string()]);
    for (_, flags, tags) in &rows {
        assert_eq!(
            *flags,
            Flags(0b0010),
            "whole-item fan-out hits every membership"
        );
        assert_eq!(
            *tags, want,
            "whole-item write replaces tags on every membership"
        );
    }
    assert_eq!(store.get_email(TEST_ACCOUNT, id).unwrap().tags, want);
}

/// Create-path user keywords (`Tags`) thread atomically through
/// `create_email` / `create_email_with_memberships` — the IMAP APPEND
/// (W2) and JMAP `Email/set create` (W3) seam. Initial tags land on
/// every fresh membership in the SAME transaction as the create (mds
/// `insert_membership` writes the tags column), not a follow-up write.
#[test]
fn create_email_threads_initial_tags() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));
    let archive = mk_container(&mds, &set, "Archive", Some("\\Archive"));

    // Single-mailbox create carrying an initial user keyword.
    let (h1, _) = put_blob(&mds, b"single create with tag");
    let (id1, _uid) = store
        .create_email(
            TEST_ACCOUNT,
            inbox,
            h1,
            sample_envelope(),
            &[],
            Flags(0b0001),
            Tags::from(vec!["public".to_string()]),
            1_730_000_000_000,
        )
        .unwrap();
    let rec1 = store.get_email(TEST_ACCOUNT, id1).unwrap();
    assert_eq!(rec1.keywords, Flags(0b0001));
    assert_eq!(rec1.tags, Tags::from(vec!["public".to_string()]));

    // Multi-mailbox create: one uniform tag set lands on every membership.
    let (h2, _) = put_blob(&mds, b"multi create with tags");
    let want = Tags::from(vec!["draft".to_string(), "pinned".to_string()]);
    let (id2, _uids) = store
        .create_email_with_memberships(
            TEST_ACCOUNT,
            &[inbox, archive],
            h2,
            sample_envelope(),
            &[],
            Flags(0),
            want.clone(),
            1_730_000_000_001,
        )
        .unwrap();
    let rows = mds.item_memberships(&set, &id2).unwrap();
    assert_eq!(rows.len(), 2);
    for (_, _, tags) in &rows {
        assert_eq!(
            *tags, want,
            "uniform initial tags on every fresh membership"
        );
    }
    assert_eq!(store.get_email(TEST_ACCOUNT, id2).unwrap().tags, want);
}

/// P6 (`imap-runway.md`) — MODSEQ monotonicity per mailbox. IMAP
/// non-negotiable invariant 4: `MODSEQ` is per-mailbox and never
/// decreases. CONDSTORE depends on this for `HIGHESTMODSEQ` /
/// `STORE UNCHANGEDSINCE` semantics.
///
/// Two distinct primitives, two distinct contracts:
/// - Per-message `MODSEQ` sources from `membership.change_seq`,
///   bumped on every flag change to that specific row.
/// - Per-mailbox `HIGHESTMODSEQ` sources from `container.change_seq`,
///   bumped on every membership add / remove / flag change.
///   Sourcing it from `MAX(membership.change_seq)` is wrong:
///   `cosmix-mds::container::remove_membership_inner` deletes the
///   membership row before bumping the container counter, so the
///   per-membership max over surviving rows can *decrease* across
///   EXPUNGE — non-monotonic, violates RFC 7162.
///
/// The contract this test guards: across N `MailStore` mutations
/// against mailbox M (including `delete_email` / `move_message`
/// removals), the observed `MailboxRecord.highest_mod_seq`
/// (= `container.change_seq`) is strictly increasing across the
/// N call boundaries; per-message `MODSEQ` is strictly increasing
/// for any single membership across mutations to itself.
#[test]
fn imap_contract_modseq_monotonicity_per_mailbox() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));
    let sibling = mk_container(&mds, &set, "Other", None);

    // Helper: read the per-mailbox HIGHESTMODSEQ via the MailStore
    // projection so we're testing the surfaced field, not the raw mds
    // counter.
    let highest_mod_seq = |c: ContainerId| -> u64 {
        store
            .list_mailboxes(TEST_ACCOUNT)
            .unwrap()
            .into_iter()
            .find(|m| m.id == c)
            .unwrap()
            .highest_mod_seq
    };

    let mut prev = highest_mod_seq(inbox);

    // Op 1: add an item to INBOX.
    let id_a = add_item_in(&mds, &set, inbox, b"modseq-a");
    let after_add = highest_mod_seq(inbox);
    assert!(
        after_add > prev,
        "add must bump container HIGHESTMODSEQ: {prev} -> {after_add}"
    );
    prev = after_add;

    // Op 2: flag flip on the INBOX membership via the IMAP-shaped path.
    store
        .set_keywords_in_mailbox(TEST_ACCOUNT, id_a, inbox, Flags(0b0001), Tags::new())
        .unwrap();
    let after_flag = highest_mod_seq(inbox);
    assert!(
        after_flag > prev,
        "flag must bump container HIGHESTMODSEQ: {prev} -> {after_flag}"
    );
    prev = after_flag;

    // Op 3: add a second item — covers the "MAX(membership.change_seq)
    // would be non-monotonic across EXPUNGE" regression: even with
    // multiple memberships the source-of-truth is the container-level
    // counter.
    let id_b = add_item_in(&mds, &set, inbox, b"modseq-b");
    let after_add_b = highest_mod_seq(inbox);
    assert!(after_add_b > prev);
    prev = after_add_b;

    // Op 4: remove the second item (EXPUNGE shape) — the
    // per-membership max drops, but the per-container counter must
    // strictly increase. This is the load-bearing test for P6's
    // "sourcing HIGHESTMODSEQ from `MAX(membership.change_seq)` is
    // explicitly excluded" rule.
    mds.remove_membership(&set, &id_b, &inbox).unwrap();
    let after_remove = highest_mod_seq(inbox);
    assert!(
        after_remove > prev,
        "remove must still bump container HIGHESTMODSEQ even though it \
         drops the highest membership.change_seq row: {prev} -> {after_remove}"
    );

    // Per-message MODSEQ on the surviving membership must reflect the
    // earlier flag write, and a fresh flag write must bump it again.
    let handle = store
        .list_emails_in_mailbox(TEST_ACCOUNT, inbox, ListOpts::default())
        .unwrap()
        .into_iter()
        .find(|h| h.id == id_a)
        .unwrap();
    let mod_seq_before = handle.mod_seq;
    store
        .set_keywords_in_mailbox(TEST_ACCOUNT, id_a, inbox, Flags(0b0011), Tags::new())
        .unwrap();
    let handle = store
        .list_emails_in_mailbox(TEST_ACCOUNT, inbox, ListOpts::default())
        .unwrap()
        .into_iter()
        .find(|h| h.id == id_a)
        .unwrap();
    assert!(
        handle.mod_seq > mod_seq_before,
        "per-message MODSEQ must strictly increase on flag mutation: \
         {mod_seq_before} -> {mod_seq_after}",
        mod_seq_after = handle.mod_seq,
    );

    // Sibling-mailbox isolation: a mutation in `sibling` does not
    // touch INBOX's HIGHESTMODSEQ. (Confirms the per-mailbox keying
    // of the counter, not just monotonicity.)
    let inbox_before = highest_mod_seq(inbox);
    let _id_other = add_item_in(&mds, &set, sibling, b"sibling");
    let inbox_after = highest_mod_seq(inbox);
    assert_eq!(
        inbox_before, inbox_after,
        "mutation in a sibling mailbox must NOT bump INBOX HIGHESTMODSEQ"
    );
}

/// P6 (`imap-runway.md`) — UIDVALIDITY stable across mailbox
/// lifecycle. IMAP non-negotiable invariants 1, 10:
/// UIDVALIDITY is stable for the life of a mailbox/container;
/// message immutables (RFC822.SIZE, INTERNALDATE, ENVELOPE,
/// BODYSTRUCTURE) must not change post-insert.
///
/// `container.seq_validity` is allocated once at container
/// creation in mds. The contract this test guards: no
/// `MailStore` mutation path can rotate `seq_validity` on the
/// happy path; a UIDVALIDITY rotation is reserved for the
/// explicit "rebuild" path (which IMAP RFC 9051 §2.3.1.1
/// calls out as an exceptional condition the client expects to
/// re-sync on).
#[test]
fn imap_contract_uidvalidity_stable_across_lifecycle() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));
    let sibling = mk_container(&mds, &set, "Trash", Some("\\Trash"));

    let uv0 = store
        .list_mailboxes(TEST_ACCOUNT)
        .unwrap()
        .into_iter()
        .find(|m| m.id == inbox)
        .unwrap()
        .uid_validity;

    let id = add_item_in(&mds, &set, inbox, b"uidvalidity test");
    store
        .set_keywords_in_mailbox(TEST_ACCOUNT, id, inbox, Flags(0b0001), Tags::new())
        .unwrap();
    mds.move_item(&set, &id, &inbox, &sibling, Flags(0))
        .unwrap();
    mds.remove_membership(&set, &id, &sibling).unwrap();

    let uv1 = store
        .list_mailboxes(TEST_ACCOUNT)
        .unwrap()
        .into_iter()
        .find(|m| m.id == inbox)
        .unwrap()
        .uid_validity;
    assert_eq!(
        uv0, uv1,
        "UIDVALIDITY must remain stable across the lifecycle"
    );
}

/// P6 + P7 (`imap-runway.md`) — UID stability + monotonicity per
/// mailbox. IMAP non-negotiable invariant 2: UID values are
/// per-mailbox, monotonically increasing, never reused.
///
/// The invariant itself is a P6 substrate concern (the
/// `membership.seq` primitive must never recycle); P7 surfaces
/// the freshly-allocated UID through `MailStore` return values
/// for UIDPLUS / APPENDUID / COPYUID. This stub can be unignored
/// once P6 lands (UIDs become readable on `EmailHandle`); the
/// assertion sharpens further once P7 lands and the new UID is
/// available directly from the create/move return value.
#[test]
fn imap_contract_uid_monotonic_no_reuse() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    let id_a = add_item_in(&mds, &set, inbox, b"uid-a");
    let uid_a = store
        .list_emails_in_mailbox(TEST_ACCOUNT, inbox, ListOpts::default())
        .unwrap()
        .into_iter()
        .find(|h| h.id == id_a)
        .unwrap()
        .uid;
    assert!(uid_a > 0, "membership.seq is 1-based; UID must be >= 1");

    mds.remove_membership(&set, &id_a, &inbox).unwrap();

    let id_b = add_item_in(&mds, &set, inbox, b"uid-b");
    let uid_b = store
        .list_emails_in_mailbox(TEST_ACCOUNT, inbox, ListOpts::default())
        .unwrap()
        .into_iter()
        .find(|h| h.id == id_b)
        .unwrap()
        .uid;

    assert!(
        uid_b > uid_a,
        "UID must never recycle within a mailbox: uid_a={uid_a}, uid_b={uid_b}"
    );
}

/// IMAP Phase 2 prereq — `MailboxRecord.uid_next` surfaces
/// `container.next_seq` and is monotonically non-decreasing.
/// SELECT/EXAMINE/STATUS responses depend on this field; if
/// it regressed (e.g. a stale snapshot, or a wrong source
/// column) the client would advertise the wrong UID for the
/// next APPEND and corrupt resync state.
#[test]
fn imap_contract_uid_next_monotonic_and_surfaced() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    let uid_next = |c: ContainerId| -> u64 {
        store
            .list_mailboxes(TEST_ACCOUNT)
            .unwrap()
            .into_iter()
            .find(|m| m.id == c)
            .unwrap()
            .uid_next
    };

    let n0 = uid_next(inbox);
    assert_eq!(n0, 1, "empty mailbox starts at UIDNEXT=1");

    let id_a = add_item_in(&mds, &set, inbox, b"uidnext-a");
    let n1 = uid_next(inbox);
    assert_eq!(n1, 2, "first insert allocates UID 1, UIDNEXT advances to 2");

    // Remove the only membership — UIDNEXT must not retreat
    // (no UID reuse across EXPUNGE; mirrors the
    // `imap_contract_uid_monotonic_no_reuse` invariant from
    // the read-side angle).
    mds.remove_membership(&set, &id_a, &inbox).unwrap();
    let n2 = uid_next(inbox);
    assert_eq!(n2, n1, "UIDNEXT must not regress on EXPUNGE: {n1} -> {n2}");

    let _id_b = add_item_in(&mds, &set, inbox, b"uidnext-b");
    let n3 = uid_next(inbox);
    assert_eq!(
        n3,
        n2 + 1,
        "second insert bumps UIDNEXT once more: {n2} -> {n3}"
    );
}

/// P5 (`imap-runway.md`) — IDLE notifier surface. IMAP
/// non-negotiable invariant 8: IDLE publishes only after the
/// storage transaction commits. The mds notifier already fires
/// post-commit by construction (`BufferedEvents::drain_into` is
/// invoked from `with_set_tx` after `tx.commit()`; see
/// `cosmix-mds::store::with_set_tx`).
///
/// The contract this test guards: `MailStore::subscribe_mailbox(
/// account, mailbox)` must surface the underlying `cosmix-mds`
/// per-`(SetId, ContainerId)` `broadcast::Receiver<ContainerEvent>`
/// without leaking `SetId` to the IMAP layer; events fire
/// post-commit only, with delivery latency <100 ms in the
/// in-process case.
#[test]
fn imap_contract_idle_notifier_emits_on_membership_change() {
    use cosmix_mds::ContainerEvent;
    use tokio::sync::broadcast::error::TryRecvError;

    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let m = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));
    let n = mk_container(&mds, &set, "Other", None);

    // A subscribes to BOTH mailboxes — sibling subscription is how we
    // detect cross-mailbox event leakage (the contract's "keyed
    // per-mailbox, not per-account" clause).
    let mut rx_m = store.subscribe_mailbox(TEST_ACCOUNT, m).unwrap();
    let mut rx_n = store.subscribe_mailbox(TEST_ACCOUNT, n).unwrap();

    // Helper: drain rx_m / rx_n into Vec; loops because broadcast
    // delivers eagerly post-commit but `try_recv` is per-message.
    fn drain(rx: &mut tokio::sync::broadcast::Receiver<ContainerEvent>) -> Vec<ContainerEvent> {
        let mut out = Vec::new();
        loop {
            match rx.try_recv() {
                Ok(ev) => out.push(ev),
                Err(TryRecvError::Empty) => return out,
                Err(TryRecvError::Closed) => return out,
                Err(TryRecvError::Lagged(_)) => continue,
            }
        }
    }

    // (1) ItemAdded into M — rx_m sees it, rx_n MUST NOT.
    let id = add_item_in(&mds, &set, m, b"idle-payload-1");
    let events_m = drain(&mut rx_m);
    let events_n = drain(&mut rx_n);
    assert!(
        events_m
            .iter()
            .any(|e| matches!(e, ContainerEvent::ItemAdded { item_id, .. } if *item_id == id)),
        "rx_m must receive ItemAdded post-commit; got {events_m:?}"
    );
    assert!(
        events_n.is_empty(),
        "sibling-mailbox subscription must NOT receive ItemAdded fired against M; got {events_n:?}"
    );

    // (2) FlagsChanged via set_keywords_in_mailbox — rx_m only.
    store
        .set_keywords_in_mailbox(TEST_ACCOUNT, id, m, Flags(0b0001), Tags::new())
        .unwrap();
    let events_m = drain(&mut rx_m);
    let events_n = drain(&mut rx_n);
    assert!(
        events_m
            .iter()
            .any(|e| matches!(e, ContainerEvent::FlagsChanged { item_id, .. } if *item_id == id)),
        "rx_m must receive FlagsChanged post-commit; got {events_m:?}"
    );
    assert!(
        events_n.is_empty(),
        "sibling-mailbox subscription must NOT receive FlagsChanged on M; got {events_n:?}"
    );

    // (3) Sibling mutation — rx_n receives, rx_m MUST NOT.
    let sibling_id = add_item_in(&mds, &set, n, b"idle-payload-sibling");
    let events_m = drain(&mut rx_m);
    let events_n = drain(&mut rx_n);
    assert!(
        events_n.iter().any(
            |e| matches!(e, ContainerEvent::ItemAdded { item_id, .. } if *item_id == sibling_id)
        ),
        "rx_n must receive its own ItemAdded; got {events_n:?}"
    );
    assert!(
        events_m.is_empty(),
        "mutation in N must not wake M's subscription (IMAP non-negotiable invariant 8 / per-mailbox keying); got {events_m:?}"
    );

    // (4) Rollback — events buffered inside a `with_set_tx` closure
    // that returns Err MUST NOT replay. This is the load-bearing
    // post-commit-only guarantee for IDLE: an IMAP client must never
    // see a notification for a write that never landed.
    let result: cosmix_mds::Result<()> = mds.with_set_tx(&set, |tx| {
        tx.store_flags(&id, &m, Flags(0b1111))?;
        // Roll back via a non-DB-corrupting error sentinel.
        Err(cosmix_mds::Error::Other("idle-rollback-probe".into()))
    });
    assert!(result.is_err(), "rollback probe must surface its Err");
    let events_m = drain(&mut rx_m);
    let events_n = drain(&mut rx_n);
    assert!(
        events_m.is_empty(),
        "rolled-back FlagsChanged must NOT fire on rx_m; got {events_m:?}"
    );
    assert!(
        events_n.is_empty(),
        "rolled-back FlagsChanged must NOT fire on rx_n; got {events_n:?}"
    );

    // The flag mutation that rolled back must not have landed.
    let rows = mds.item_memberships(&set, &id).unwrap();
    let inbox_flags = rows.iter().find(|(c, _, _)| *c == m).unwrap().1;
    assert_eq!(
        inbox_flags,
        Flags(0b0001),
        "rolled-back store_flags must not persist; INBOX retains the prior \\Seen-only value"
    );

    // Subscribing to a missing mailbox surfaces "mailbox not found" —
    // matches the wider read-path sentinel and prevents lazy-channel
    // allocation for typo'd containers.
    let err = store
        .subscribe_mailbox(TEST_ACCOUNT, ContainerId(uuid::Uuid::new_v4()))
        .unwrap_err();
    let msg = format!("{err}");
    assert!(msg.starts_with("mailbox not found"), "got: {msg}");
}

// ---- MCS Phase 3-B — stale-token rejection on the read path ----
//
// `Mds::prune_changelog` advances the per-stream retention floor; any
// `sinceState` strictly below that floor can no longer be answered
// from the surviving rows. The mailstore read path must surface that
// as the `"stale since_state"` prefix so the JMAP handler maps it to
// `cannotCalculateChanges` (RFC 8620 §5.2). The predicate is strict
// `<` — `since == floor` is still answerable from `seq > since` rows.

#[test]
fn email_changes_stale_since_state_below_set_change_floor_errors() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    // Drive a few set_change rows so the prune has something to cut.
    for i in 0..4u8 {
        let (hash, _) = put_blob(&mds, &[i, i, i]);
        let _ = store
            .create_email(
                TEST_ACCOUNT,
                inbox,
                hash,
                sample_envelope(),
                &[],
                Flags(0),
                Tags::new(),
                1_700_000_000_000 + i as i64,
            )
            .unwrap();
    }

    // Sample the head so we can compare floor afterwards.
    let head_before = store.email_changes(TEST_ACCOUNT, 0).unwrap().new_state;
    assert!(head_before > 0);

    // Prune to keep only the newest 1 row — floor jumps just above 0
    // and at most one increment below `head_before`.
    let report = mds
        .prune_changelog(&set, ChangelogStream::SetChange, 1)
        .unwrap();
    assert!(report.new_floor > 0);
    let floor = report.new_floor;

    // since == floor still answers (strict-< predicate).
    let at_floor = store.email_changes(TEST_ACCOUNT, floor).unwrap();
    assert_eq!(at_floor.new_state, head_before);

    // since == floor - 1 is stale and must surface with the
    // `"stale since_state"` prefix.
    let stale = floor.saturating_sub(1);
    let err = store
        .email_changes(TEST_ACCOUNT, stale)
        .unwrap_err()
        .to_string();
    assert!(
        err.starts_with("stale since_state"),
        "expected `stale since_state` prefix, got: {err}"
    );
}

#[test]
fn mailbox_changes_stale_since_state_below_lifecycle_floor_errors() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);

    // Drive a few container_change_set rows (lifecycle stream) so
    // the prune has rows to cut.
    let _ = mk_container(&mds, &set, "Folder-A", None);
    let _ = mk_container(&mds, &set, "Folder-B", None);
    let _ = mk_container(&mds, &set, "Folder-C", None);

    let report = mds
        .prune_changelog(&set, ChangelogStream::ContainerChangeSet, 1)
        .unwrap();
    assert!(report.new_floor > 0);
    let floor = report.new_floor;
    let stale = floor.saturating_sub(1);

    // Paired token: lifecycle half stale, set_change half at 0.
    let err = store
        .mailbox_changes(TEST_ACCOUNT, &format!("{stale}:0"), 100)
        .expect_err("lifecycle stale half should be rejected")
        .to_string();
    assert!(
        err.starts_with("stale since_state"),
        "expected `stale since_state` prefix, got: {err}"
    );

    // The token at the floor still answers — `seq > floor` rows survived.
    let at_floor = store
        .mailbox_changes(TEST_ACCOUNT, &format!("{floor}:0"), 100)
        .expect("token at lifecycle floor should still be answerable");
    // The remaining lifecycle row(s) above the floor surface as
    // `created`.
    assert!(!at_floor.created.is_empty() || !at_floor.updated.is_empty());
}

#[test]
fn mailbox_changes_stale_since_state_below_set_change_floor_errors() {
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

    // Drive a few set_change rows on the item-event stream.
    for i in 0..3u8 {
        let (hash, _) = put_blob(&mds, &[i + 10, i + 10]);
        let _ = store
            .create_email(
                TEST_ACCOUNT,
                inbox,
                hash,
                sample_envelope(),
                &[],
                Flags(0),
                Tags::new(),
                1_700_000_000_000 + i as i64,
            )
            .unwrap();
    }

    let report = mds
        .prune_changelog(&set, ChangelogStream::SetChange, 1)
        .unwrap();
    assert!(report.new_floor > 0);
    let stale = report.new_floor.saturating_sub(1);

    // Paired token: lifecycle half at 0, set_change half stale.
    let err = store
        .mailbox_changes(TEST_ACCOUNT, &format!("0:{stale}"), 100)
        .expect_err("set_change stale half should be rejected")
        .to_string();
    assert!(
        err.starts_with("stale since_state"),
        "expected `stale since_state` prefix, got: {err}"
    );
}

#[test]
fn mailbox_changes_keep_n_zero_lifecycle_prune_preserves_floor_equality() {
    // `prune_changelog(ContainerChangeSet, 0)` deletes every lifecycle
    // row, dropping `MAX(container_change_set_seq)` to 0 while the
    // floor retains the previous max. Without lifting
    // `curr_lifecycle = max(MAX, floor)` and lifting `mailbox_state`'s
    // emitted lifecycle half the same way, two contracts break:
    //   (a) `since == floor` would get rejected as `"future
    //       since_state"` (head=0, since=floor>0) before the stale
    //       check fires, violating the strict-`<` predicate.
    //   (b) `mailbox_state` would emit `"0:<set_change>"` — a token
    //       below the floor — so the next `Mailbox/changes` against
    //       the server-issued token would reject as stale.
    let (_d, mds, store) = open_mailstore();
    let set = provision(&store);
    let _ = mk_container(&mds, &set, "Folder-A", None);
    let _ = mk_container(&mds, &set, "Folder-B", None);

    // Wipe the lifecycle stream entirely.
    let report = mds
        .prune_changelog(&set, ChangelogStream::ContainerChangeSet, 0)
        .unwrap();
    let floor = report.new_floor;
    assert!(floor > 0);

    // (a) `since == floor` is answerable (no surviving rows above
    // floor, but the query is still valid — empty delta).
    let at_floor = store
        .mailbox_changes(TEST_ACCOUNT, &format!("{floor}:0"), 100)
        .expect("since == lifecycle floor must be answerable after keep_n=0 prune");
    assert!(at_floor.created.is_empty());
    assert!(at_floor.updated.is_empty());
    assert!(at_floor.destroyed.is_empty());

    // `since == floor - 1` is below the floor → stale.
    let stale = floor.saturating_sub(1);
    let err = store
        .mailbox_changes(TEST_ACCOUNT, &format!("{stale}:0"), 100)
        .expect_err("lifecycle since below floor must be stale")
        .to_string();
    assert!(
        err.starts_with("stale since_state"),
        "expected `stale since_state` prefix, got: {err}"
    );

    // (b) The emitted state token's lifecycle half must be `>= floor`.
    let state = store.mailbox_state(TEST_ACCOUNT).unwrap();
    let (l, _s) = state.split_once(':').expect("paired state token");
    let lifecycle_half: u64 = l.parse().expect("lifecycle half decimal");
    assert!(
        lifecycle_half >= floor,
        "mailbox_state lifecycle half {lifecycle_half} must be >= floor {floor}"
    );
}

// ==================== retrain drain worker ====================
//
// IMAP junk-boundary retrain parity with JMAP. These exercise the
// zero-migration design: `INSERT OR REPLACE` enqueue (mints a fresh
// rowid per re-drag) + `ORDER BY rowid ASC` drain + rowid-keyed
// finalise. `record_label`'s per-stamp latest-wins reversal then
// converges to the correct final corpus for every in/out/in
// interleave — a message that ends in Junk must end up Spam in the
// corpus, matching JMAP's inline path.

mod retrain_drain {
    use super::*;
    use cosmix_maild_bayesian::{
        ClassifierConfig, DefaultClassifier,
        classifier::Classifier,
        storage::{AccountConnection, SqliteAccountConnection, StorageBackend},
    };
    use cosmix_maild_rules::AccountId as RetrainAccountId;
    use std::path::Path;

    use crate::mailstore::retrain::{self, RetrainOutboxWorker};

    /// Single in-memory bayes connection shared across accounts —
    /// hermetic, mirrors the `bus/bayesian.rs` harness. The worker
    /// always addresses account "1" (TEST_ACCOUNT), and `stats()`
    /// reads the same account, so one connection is sufficient.
    struct SingleConnBackend(Arc<SqliteAccountConnection>);

    #[async_trait::async_trait]
    impl StorageBackend for SingleConnBackend {
        async fn open_account(
            &self,
            _account: &RetrainAccountId,
        ) -> cosmix_maild_bayesian::Result<Arc<dyn AccountConnection>> {
            Ok(self.0.clone() as Arc<dyn AccountConnection>)
        }
    }

    fn mk_classifier() -> Arc<DefaultClassifier> {
        let conn = Arc::new(SqliteAccountConnection::open_path(Path::new(":memory:"), 0).unwrap());
        let backend: Arc<dyn StorageBackend> = Arc::new(SingleConnBackend(conn));
        // cold_start_floor = 0 so the corpus reads deterministically
        // after a single retrain (cold-start thresholds are
        // classify-only, irrelevant to a retrain-count assertion).
        let cfg = ClassifierConfig {
            cold_start_floor: 0,
            ..ClassifierConfig::default()
        };
        Arc::new(DefaultClassifier::new(cfg, backend))
    }

    async fn corpus_counts(cls: &Arc<DefaultClassifier>) -> (u32, u32) {
        let s = cls.stats(&RetrainAccountId::new("1")).await.unwrap();
        (s.spam_messages, s.ham_messages)
    }

    // Label-qualified: a stamp can have both a (junk) and a (ham)
    // row pending simultaneously (the interleave case), so a
    // stamp-only lookup would non-deterministically read either.
    fn outbox_attempts_for(
        mds: &SqliteCasMds,
        set: &SetId,
        stamp_id: &str,
        label: &str,
    ) -> Option<(i64, Option<String>)> {
        let mut out = None;
        let _: cosmix_mds::Result<()> = mds.with_set_tx(set, |tx| {
            out = tx
                .tx()
                .query_row(
                    "SELECT attempts, last_error FROM mail_retrain_outbox \
                     WHERE stamp_id = ?1 AND label = ?2",
                    params![stamp_id, label],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .ok();
            Err(cosmix_mds::Error::Other("probe".into()))
        });
        out
    }

    fn outbox_rowids(mds: &SqliteCasMds, set: &SetId) -> Vec<i64> {
        let mut out = Vec::new();
        let _: cosmix_mds::Result<()> = mds.with_set_tx(set, |tx| {
            let mut stmt = tx
                .tx()
                .prepare("SELECT rowid FROM mail_retrain_outbox ORDER BY rowid ASC")
                .unwrap();
            out = stmt
                .query_map([], |r| r.get(0))
                .unwrap()
                .collect::<rusqlite::Result<Vec<i64>>>()
                .unwrap();
            Err(cosmix_mds::Error::Other("probe".into()))
        });
        out
    }

    // B1 — the interleave Codex flagged. In→Junk, Junk→In, In→Junk
    // before any drain. `OR IGNORE` would drop the 2nd In→Junk and
    // the corpus would wrongly end Ham; `OR REPLACE` + rowid-order
    // drain must converge to Spam (the message ends in Junk).
    #[tokio::test]
    async fn b1_interleave_in_out_in_converges_to_spam() {
        let (_d, mds, store) = open_mailstore();
        let set = provision(&store);
        let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));
        let junk = mk_container(&mds, &set, "Junk", Some("\\Junk"));
        let item = add_item_in(&mds, &set, inbox, b"interleave-msg");

        store
            .move_message(TEST_ACCOUNT, item, inbox, junk, Flags(0), "s1")
            .unwrap();
        store
            .move_message(TEST_ACCOUNT, item, junk, inbox, Flags(0), "s1")
            .unwrap();
        store
            .move_message(TEST_ACCOUNT, item, inbox, junk, Flags(0), "s1")
            .unwrap();

        // Exactly two rows survive: (s1,ham) then (s1,junk), the
        // latter on a fresh rowid > the ham row.
        let rowids = outbox_rowids(&mds, &set);
        assert_eq!(rowids.len(), 2, "ham + replaced-junk rows expected");
        assert!(rowids[0] < rowids[1], "junk re-drag must sort last");

        let cls = mk_classifier();
        let worker = RetrainOutboxWorker::new(Arc::clone(&mds), Arc::clone(&cls));
        let applied = worker.drain_once().await.unwrap();
        assert_eq!(applied, 2);

        let (spam, ham) = corpus_counts(&cls).await;
        assert_eq!(
            (spam, ham),
            (1, 0),
            "message ends in Junk → corpus must be Spam (JMAP parity)"
        );
        assert_eq!(count_outbox(&mds, &set), 0, "rows drained");
    }

    // A re-drag pair (out then in) after a successful drain must
    // converge back to Spam with no net double-count, and a second
    // empty tick must not change anything (at-least-once safety).
    #[tokio::test]
    async fn idempotent_redrain_no_double_count() {
        let (_d, mds, store) = open_mailstore();
        let set = provision(&store);
        let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));
        let junk = mk_container(&mds, &set, "Junk", Some("\\Junk"));
        let item = add_item_in(&mds, &set, inbox, b"idem-msg");
        let cls = mk_classifier();
        let worker = RetrainOutboxWorker::new(Arc::clone(&mds), Arc::clone(&cls));

        store
            .move_message(TEST_ACCOUNT, item, inbox, junk, Flags(0), "idem")
            .unwrap();
        assert_eq!(worker.drain_once().await.unwrap(), 1);
        assert_eq!(corpus_counts(&cls).await, (1, 0));

        // Re-drag out then back in, then drain again.
        store
            .move_message(TEST_ACCOUNT, item, junk, inbox, Flags(0), "idem")
            .unwrap();
        store
            .move_message(TEST_ACCOUNT, item, inbox, junk, Flags(0), "idem")
            .unwrap();
        worker.drain_once().await.unwrap();
        assert_eq!(
            corpus_counts(&cls).await,
            (1, 0),
            "convergence is idempotent — still exactly one Spam"
        );

        // A spurious extra tick on an empty outbox is a no-op.
        assert_eq!(worker.drain_once().await.unwrap(), 0);
        assert_eq!(corpus_counts(&cls).await, (1, 0));
    }

    // Post-v1.8: hard-deleting the message before the drain reaps the
    // outbox row too, via the `mail_retrain_outbox.item_id` FK cascade.
    // The pre-v1.8 dead-letter park-at-MAX_ATTEMPTS scenario is no
    // longer reachable through item deletion (the row vanishes with
    // the item), so the drain is simply a clean no-op with no surviving
    // row to park or retry. The "message gone before drain" defensive
    // branch in `retrain.rs` remains for any non-item-delete orphaning
    // path (e.g. a future blob-only GC), but is unreachable here.
    #[tokio::test]
    async fn deleting_message_cascades_outbox_row_no_dead_letter() {
        let (_d, mds, store) = open_mailstore();
        let set = provision(&store);
        let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));
        let junk = mk_container(&mds, &set, "Junk", Some("\\Junk"));
        let item = add_item_in(&mds, &set, inbox, b"doomed");

        store
            .move_message(TEST_ACCOUNT, item, inbox, junk, Flags(0), "dead")
            .unwrap();
        // Outbox row enqueued by the \Junk move.
        assert_eq!(count_outbox(&mds, &set), 1);

        // Drop the sole (junk) membership → item deleted → FK cascade
        // reaps the outbox row.
        mds.remove_membership(&set, &item, &junk).unwrap();
        assert_eq!(
            count_outbox(&mds, &set),
            0,
            "v1.8 FK cascade must reap the outbox row with the item"
        );
        assert!(
            outbox_attempts_for(&mds, &set, "dead", "junk").is_none(),
            "no surviving outbox row to dead-letter"
        );

        let cls = mk_classifier();
        let worker = RetrainOutboxWorker::new(Arc::clone(&mds), Arc::clone(&cls));
        // Nothing to drain — clean no-op, no training applied, no retry.
        assert_eq!(worker.drain_once().await.unwrap(), 0, "nothing applied");
        assert_eq!(corpus_counts(&cls).await, (0, 0), "no training applied");
        assert_eq!(worker.drain_once().await.unwrap(), 0, "still nothing");
    }

    // Dead-letter still applies to the *blob-only* orphaning case: the
    // `item` row survives (so the FK does NOT cascade the outbox row),
    // but the underlying blob was GC'd, so `get_blob` returns
    // `BlobNotFound`. The row must park at MAX_ATTEMPTS with the
    // recorded reason and never retry — the v1.8 FK cascade narrows the
    // dead-letter window to this path, but does not close it.
    #[tokio::test]
    async fn missing_blob_with_live_item_dead_letters_without_infinite_retry() {
        let (_d, mds, store) = open_mailstore();
        let set = provision(&store);
        let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));
        let junk = mk_container(&mds, &set, "Junk", Some("\\Junk"));
        // Capture the blob hash so we can delete its CAS file later.
        let (hash, _) = put_blob(&mds, b"orphan-blob");
        let m = Membership {
            container: inbox,
            flags: Flags(0),
            added_at: 0,
        };
        let item = mds.add_item(&set, &hash, &[m]).unwrap().item_id;

        store
            .move_message(TEST_ACCOUNT, item, inbox, junk, Flags(0), "dead")
            .unwrap();
        // Item still lives in Junk → outbox row survives.
        assert_eq!(count_outbox(&mds, &set), 1);

        // Delete the blob file directly so get_blob → BlobNotFound while
        // the item row remains (simulates a blob GC'd out from under a
        // still-referenced item — the non-cascade orphaning path).
        let blob_file = cosmix_mds::blob::blob_path(&_d.path().join("blobs"), &hash);
        std::fs::remove_file(&blob_file).unwrap();

        let cls = mk_classifier();
        let worker = RetrainOutboxWorker::new(Arc::clone(&mds), Arc::clone(&cls));
        assert_eq!(worker.drain_once().await.unwrap(), 0, "nothing applied");

        let (attempts, last_err) =
            outbox_attempts_for(&mds, &set, "dead", "junk").expect("row still present");
        assert_eq!(attempts, retrain::MAX_ATTEMPTS, "parked at max");
        assert_eq!(last_err.as_deref(), Some("message gone before drain"));
        assert_eq!(corpus_counts(&cls).await, (0, 0), "no training applied");

        // A second pass must skip it (attempts not < MAX) — no retry.
        assert_eq!(worker.drain_once().await.unwrap(), 0);
        let (attempts2, _) = outbox_attempts_for(&mds, &set, "dead", "junk").unwrap();
        assert_eq!(attempts2, retrain::MAX_ATTEMPTS, "still parked, not bumped");
    }

    // Re-drag-during-drain guard. `finalise` keys its DELETE on the
    // exact rowid claimed. If a re-drag replaces the row between
    // claim and finalise, the replacement carries a *new* rowid; the
    // stale-rowid DELETE must be a no-op so the re-drag survives and
    // is reprocessed. (If finalise keyed on (stamp,label) instead it
    // would wrongly delete the fresh re-drag and lose the signal.)
    #[tokio::test]
    async fn rowid_guard_preserves_redrag_between_claim_and_finalise() {
        let (_d, mds, store) = open_mailstore();
        let set = provision(&store);
        let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));
        let junk = mk_container(&mds, &set, "Junk", Some("\\Junk"));
        let item = add_item_in(&mds, &set, inbox, b"guard-msg");

        // Initial In→Junk → row (g,junk)@R1.
        store
            .move_message(TEST_ACCOUNT, item, inbox, junk, Flags(0), "g")
            .unwrap();
        let claimed = mds.with_set_tx(&set, retrain::claim_batch).unwrap();
        assert_eq!(claimed.len(), 1);
        let r1 = claimed[0].rowid;

        // "While R1 is in flight": a re-drag (out then in). The
        // In→Junk replaces (g,junk) via OR REPLACE → R1 deleted, a
        // fresh rowid minted; a (g,ham) row is also enqueued.
        store
            .move_message(TEST_ACCOUNT, item, junk, inbox, Flags(0), "g")
            .unwrap();
        store
            .move_message(TEST_ACCOUNT, item, inbox, junk, Flags(0), "g")
            .unwrap();

        let rowids = outbox_rowids(&mds, &set);
        assert_eq!(rowids.len(), 2, "(g,ham) + replaced (g,junk)");
        assert!(!rowids.contains(&r1), "R1 was replaced away");

        // Finalise the stale claim as success — must delete 0 rows.
        mds.with_set_tx(&set, |tx| {
            retrain::finalise(tx, r1, &retrain::Finalise::Done)
        })
        .unwrap();
        assert_eq!(
            outbox_rowids(&mds, &set),
            rowids,
            "stale-rowid finalise must not touch the re-drag rows"
        );

        // Next pass reprocesses the survivors → ends Spam (in Junk).
        let cls = mk_classifier();
        let worker = RetrainOutboxWorker::new(Arc::clone(&mds), Arc::clone(&cls));
        worker.drain_once().await.unwrap();
        assert_eq!(
            corpus_counts(&cls).await,
            (1, 0),
            "re-drag survived the stale finalise and trained Spam"
        );
        assert_eq!(count_outbox(&mds, &set), 0);
    }

    // Codex MAJOR regression: a transient failure on an earlier row
    // must not let a later same-stamp opposite-label row be applied
    // first. Sequence for stamp "t": R1(junk) then R2(ham); the
    // message ends in Inbox so the corpus must end Ham. The flaky
    // backend fails the *first* `record_label` call (R1's junk).
    // Without the Retry-halt, tick 1 would apply R2(ham) and tick 2
    // would replay R1(junk) on top → wrong final Spam. With the
    // halt, R2 is deferred until R1 succeeds in order → Ham.
    struct FlakyConn {
        inner: Arc<SqliteAccountConnection>,
        fail_next: std::sync::atomic::AtomicBool,
    }

    #[async_trait::async_trait]
    impl AccountConnection for FlakyConn {
        async fn token_counts(
            &self,
            tokens: &[String],
        ) -> cosmix_maild_bayesian::Result<Vec<(String, u64, u64)>> {
            self.inner.token_counts(tokens).await
        }
        async fn record_label(
            &self,
            stamp_id: &str,
            tokens: &[String],
            label: cosmix_maild_bayesian::Label,
            cap: u32,
        ) -> cosmix_maild_bayesian::Result<Option<u64>> {
            if self
                .fail_next
                .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                return Err(cosmix_maild_bayesian::Error::Storage(
                    "synthetic transient".into(),
                ));
            }
            self.inner.record_label(stamp_id, tokens, label, cap).await
        }
        async fn forget_label(
            &self,
            stamp_id: &str,
            tokens: &[String],
        ) -> cosmix_maild_bayesian::Result<Option<cosmix_maild_bayesian::Label>> {
            self.inner.forget_label(stamp_id, tokens).await
        }
        async fn stats(
            &self,
        ) -> cosmix_maild_bayesian::Result<cosmix_maild_bayesian::AccountStats> {
            self.inner.stats().await
        }
        async fn totals(&self) -> cosmix_maild_bayesian::Result<(u64, u64)> {
            self.inner.totals().await
        }
        async fn reset(&self) -> cosmix_maild_bayesian::Result<()> {
            self.inner.reset().await
        }
        async fn snapshot(&self) -> cosmix_maild_bayesian::Result<Option<std::path::PathBuf>> {
            self.inner.snapshot().await
        }
    }

    struct ConnBackend(Arc<dyn AccountConnection>);

    #[async_trait::async_trait]
    impl StorageBackend for ConnBackend {
        async fn open_account(
            &self,
            _account: &RetrainAccountId,
        ) -> cosmix_maild_bayesian::Result<Arc<dyn AccountConnection>> {
            Ok(self.0.clone())
        }
    }

    #[tokio::test]
    async fn transient_retry_does_not_reorder_later_same_stamp_row() {
        let (_d, mds, store) = open_mailstore();
        let set = provision(&store);
        let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));
        let junk = mk_container(&mds, &set, "Junk", Some("\\Junk"));
        let item = add_item_in(&mds, &set, inbox, b"order-msg");

        // R1: In→Junk (junk).  R2: Junk→In (ham).  Ends in Inbox.
        store
            .move_message(TEST_ACCOUNT, item, inbox, junk, Flags(0), "t")
            .unwrap();
        store
            .move_message(TEST_ACCOUNT, item, junk, inbox, Flags(0), "t")
            .unwrap();

        let sqlite =
            Arc::new(SqliteAccountConnection::open_path(Path::new(":memory:"), 0).unwrap());
        let flaky: Arc<dyn AccountConnection> = Arc::new(FlakyConn {
            inner: sqlite,
            fail_next: std::sync::atomic::AtomicBool::new(true),
        });
        let backend: Arc<dyn StorageBackend> = Arc::new(ConnBackend(flaky));
        let cfg = ClassifierConfig {
            cold_start_floor: 0,
            ..ClassifierConfig::default()
        };
        let cls = Arc::new(DefaultClassifier::new(cfg, backend));
        let worker = RetrainOutboxWorker::new(Arc::clone(&mds), Arc::clone(&cls));

        // Tick 1: R1's record_label fails → Retry, batch halts before
        // R2. Nothing applied; both rows still pending.
        assert_eq!(worker.drain_once().await.unwrap(), 0);
        assert_eq!(corpus_counts(&cls).await, (0, 0));
        let (attempts, _) = outbox_attempts_for(&mds, &set, "t", "junk").expect("R1 still pending");
        assert_eq!(attempts, 1, "R1 retried once, R2 untouched");
        assert_eq!(count_outbox(&mds, &set), 2, "neither row drained yet");

        // Tick 2: R1 succeeds then R2 succeeds, in rowid order →
        // junk then ham reversal → final Ham (message is in Inbox).
        worker.drain_once().await.unwrap();
        assert_eq!(
            corpus_counts(&cls).await,
            (0, 1),
            "in-order replay after retry → Ham, not Spam"
        );
        assert_eq!(count_outbox(&mds, &set), 0);
    }

    #[test]
    fn mailbox_stats_reports_counts_roles_and_bytes() {
        let (_d, mds, store) = open_mailstore();
        let set = provision(&store);

        let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));
        let junk = mk_container(&mds, &set, "Junk", Some("\\Junk"));
        // A no-role custom folder, to prove role projects as None.
        let _work = mk_container(&mds, &set, "Work", None);

        // Two items in Inbox (10 + 20 bytes), one in Junk (5 bytes).
        add_item_in(&mds, &set, inbox, &[b'a'; 10]);
        add_item_in(&mds, &set, inbox, &[b'b'; 20]);
        add_item_in(&mds, &set, junk, &[b'c'; 5]);

        let stats = store.mailbox_stats(TEST_ACCOUNT).unwrap();
        let by_name = |n: &str| stats.iter().find(|m| m.name == n).unwrap().clone();

        let i = by_name("Inbox");
        assert_eq!(i.role, Some(MailboxRole::Inbox));
        assert_eq!(i.total_emails, 2);
        assert_eq!(i.size_bytes, 30);

        let j = by_name("Junk");
        assert_eq!(j.role, Some(MailboxRole::Junk));
        assert_eq!(j.total_emails, 1);
        assert_eq!(j.size_bytes, 5);

        let w = by_name("Work");
        assert_eq!(w.role, None);
        assert_eq!(w.total_emails, 0);
        assert_eq!(w.size_bytes, 0);
    }

    #[test]
    fn mailbox_stats_on_unprovisioned_account_is_empty() {
        let (_d, _mds, store) = open_mailstore();
        // No ensure_account_set → no set yet; must not error.
        assert!(store.mailbox_stats(TEST_ACCOUNT).unwrap().is_empty());
    }

    #[test]
    fn account_stats_dedupes_multi_homed_items() {
        let (_d, mds, store) = open_mailstore();
        let set = provision(&store);
        let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));
        let archive = mk_container(&mds, &set, "Archive", Some("\\Archive"));

        // One item filed in BOTH Inbox and Archive (multi-homed, 10 bytes)
        // plus one Inbox-only item (4 bytes).
        let (hash, _) = put_blob(&mds, &[b'x'; 10]);
        mds.add_item(
            &set,
            &hash,
            &[
                Membership {
                    container: inbox,
                    flags: Flags(0),
                    added_at: 0,
                },
                Membership {
                    container: archive,
                    flags: Flags(0),
                    added_at: 0,
                },
            ],
        )
        .unwrap();
        add_item_in(&mds, &set, inbox, &[b'y'; 4]);

        // Per-folder sums would double-count the multi-homed item
        // (Inbox=2, Archive=1 → 3); the distinct account rollup is 2.
        let stat = store.account_stats(TEST_ACCOUNT).unwrap();
        assert_eq!(stat.mailbox_count, 2);
        assert_eq!(stat.message_count, 2, "multi-homed item counted once");
        assert_eq!(stat.total_bytes, 14, "10 + 4, multi-homed counted once");
    }

    #[test]
    fn account_stats_on_unprovisioned_account_is_zero() {
        let (_d, _mds, store) = open_mailstore();
        let s = store.account_stats(TEST_ACCOUNT).unwrap();
        assert_eq!(s.mailbox_count, 0);
        assert_eq!(s.message_count, 0);
        assert_eq!(s.total_bytes, 0);
    }
}

/// Retention sweep primitive — the destructive Junk/Trash age-trim.
/// Fake-clock driven (`retention_sweep_account` takes `now_ms`), so every
/// case is deterministic. The load-bearing safety properties: keys on
/// membership `added_at` (savedbefore semantics), multi-homed messages
/// keep their other memberships, dry-run mutates nothing, the per-sweep
/// cap removes the OLDEST first and bleeds the remainder down, and the
/// FTS sidecar is reaped exactly when (and only when) an item is orphaned.
mod retention {
    use super::*;

    const DAY_MS: i64 = 86_400_000;
    /// Synthetic "now" (epoch ms, matches the timestamps other tests use).
    const NOW: i64 = 1_700_000_000_000;

    fn params(junk_days: u64, trash_days: u64, dry_run: bool) -> RetentionParams {
        RetentionParams {
            junk_retention_days: junk_days,
            trash_retention_days: trash_days,
            max_deletes_per_sweep: 5000,
            dry_run,
        }
    }

    /// Add a distinct item to one container with an explicit membership
    /// `added_at` (the retention clock). Distinct payload → distinct blob
    /// → distinct item.
    fn add_at(
        mds: &SqliteCasMds,
        set: &SetId,
        container: ContainerId,
        tag: &str,
        added_at: i64,
    ) -> ItemId {
        let (hash, _) = put_blob(mds, format!("payload-{tag}").as_bytes());
        let m = Membership {
            container,
            flags: Flags(0),
            added_at,
        };
        mds.add_item(set, &hash, &[m]).unwrap().item_id
    }

    /// Add ONE item filed in several containers, each with its own
    /// membership `added_at` (the multi-homed case).
    fn add_multi(
        mds: &SqliteCasMds,
        set: &SetId,
        placements: &[(ContainerId, i64)],
        tag: &str,
    ) -> ItemId {
        let (hash, _) = put_blob(mds, format!("multi-{tag}").as_bytes());
        let ms: Vec<Membership> = placements
            .iter()
            .map(|(c, a)| Membership {
                container: *c,
                flags: Flags(0),
                added_at: *a,
            })
            .collect();
        mds.add_item(set, &hash, &ms).unwrap().item_id
    }

    fn membership_exists(
        mds: &SqliteCasMds,
        set: &SetId,
        item: &ItemId,
        container: &ContainerId,
    ) -> bool {
        let mut found = false;
        let _: cosmix_mds::Result<()> = mds.with_set_tx(set, |tx| {
            let n: i64 = tx
                .tx()
                .query_row(
                    "SELECT COUNT(*) FROM membership WHERE item_id = ?1 AND container_id = ?2",
                    params![item.0.to_string(), container.0.to_string()],
                    |r| r.get(0),
                )
                .unwrap();
            found = n > 0;
            Err(cosmix_mds::Error::Other("probe".into()))
        });
        found
    }

    /// Force a membership's `added_at` (used to age a `create_email`-made
    /// item, which auto-stamps `added_at` to now — we need it old).
    fn age_membership(
        mds: &SqliteCasMds,
        set: &SetId,
        item: &ItemId,
        container: &ContainerId,
        added_at: i64,
    ) {
        mds.with_set_tx(set, |tx| {
            tx.tx()
                .execute(
                    "UPDATE membership SET added_at = ?1 WHERE item_id = ?2 AND container_id = ?3",
                    params![added_at, item.0.to_string(), container.0.to_string()],
                )
                .unwrap();
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn removes_aged_junk_keeps_fresh() {
        let (_d, mds, store) = open_mailstore();
        let set = provision(&store);
        let junk = mk_container(&mds, &set, "Junk", Some("\\Junk"));

        let aged = add_at(&mds, &set, junk, "aged", NOW - 30 * DAY_MS);
        let fresh = add_at(&mds, &set, junk, "fresh", NOW - DAY_MS);

        let report = store
            .retention_sweep_account(TEST_ACCOUNT, &params(7, 0, false), NOW)
            .unwrap();
        assert_eq!(report.junk_removed, 1);
        assert_eq!(report.trash_removed, 0);
        assert!(!report.junk_capped);
        assert!(
            !membership_exists(&mds, &set, &aged, &junk),
            "aged Junk membership removed"
        );
        assert!(
            membership_exists(&mds, &set, &fresh, &junk),
            "fresh Junk membership kept"
        );
        // The aged item had only the Junk membership → item row dropped.
        assert!(mds.fetch_item_meta(&set, &aged).is_err());
        assert!(mds.fetch_item_meta(&set, &fresh).is_ok());
    }

    #[test]
    fn preserves_multi_homed_item() {
        // THE multi-homed safety property: an aged Junk membership is
        // removed, but the same item's Inbox membership survives and the
        // item is NOT deleted.
        let (_d, mds, store) = open_mailstore();
        let set = provision(&store);
        let junk = mk_container(&mds, &set, "Junk", Some("\\Junk"));
        let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

        let id = add_multi(
            &mds,
            &set,
            &[(junk, NOW - 30 * DAY_MS), (inbox, NOW - 30 * DAY_MS)],
            "shared",
        );

        let report = store
            .retention_sweep_account(TEST_ACCOUNT, &params(7, 0, false), NOW)
            .unwrap();
        assert_eq!(report.junk_removed, 1);
        assert!(
            !membership_exists(&mds, &set, &id, &junk),
            "Junk membership removed"
        );
        assert!(
            membership_exists(&mds, &set, &id, &inbox),
            "Inbox membership PRESERVED"
        );
        assert!(
            mds.fetch_item_meta(&set, &id).is_ok(),
            "multi-homed item NOT deleted"
        );
    }

    #[test]
    fn dry_run_counts_but_removes_nothing() {
        let (_d, mds, store) = open_mailstore();
        let set = provision(&store);
        let junk = mk_container(&mds, &set, "Junk", Some("\\Junk"));
        let aged = add_at(&mds, &set, junk, "aged", NOW - 30 * DAY_MS);

        let report = store
            .retention_sweep_account(TEST_ACCOUNT, &params(7, 0, true), NOW)
            .unwrap();
        // Reports what WOULD be removed…
        assert_eq!(report.junk_removed, 1);
        // …but removed nothing.
        assert!(
            membership_exists(&mds, &set, &aged, &junk),
            "dry-run kept the membership"
        );
        assert!(
            mds.fetch_item_meta(&set, &aged).is_ok(),
            "dry-run kept the item"
        );
    }

    #[test]
    fn per_sweep_cap_removes_oldest_first_and_bleeds_down() {
        let (_d, mds, store) = open_mailstore();
        let set = provision(&store);
        let junk = mk_container(&mds, &set, "Junk", Some("\\Junk"));

        // Five aged items, ages 100/90/80/70/60 days (all > 7-day window).
        let ages = [100, 90, 80, 70, 60];
        let ids: Vec<ItemId> = ages
            .iter()
            .map(|d| add_at(&mds, &set, junk, &format!("a{d}"), NOW - d * DAY_MS))
            .collect();

        let mut p = params(7, 0, false);
        p.max_deletes_per_sweep = 2;
        let r1 = store
            .retention_sweep_account(TEST_ACCOUNT, &p, NOW)
            .unwrap();
        assert_eq!(r1.junk_removed, 2);
        assert!(
            r1.junk_capped,
            "more eligible than the cap → capped flag set"
        );
        // Oldest two (100d, 90d) removed; rest remain.
        assert!(!membership_exists(&mds, &set, &ids[0], &junk));
        assert!(!membership_exists(&mds, &set, &ids[1], &junk));
        assert!(membership_exists(&mds, &set, &ids[2], &junk));

        // Next sweep bleeds down the remainder.
        let r2 = store
            .retention_sweep_account(TEST_ACCOUNT, &p, NOW)
            .unwrap();
        assert_eq!(r2.junk_removed, 2);
        assert!(r2.junk_capped);
        assert!(!membership_exists(&mds, &set, &ids[2], &junk));
        assert!(!membership_exists(&mds, &set, &ids[3], &junk));
        assert!(membership_exists(&mds, &set, &ids[4], &junk));

        // Final sweep clears the last one; nothing left → not capped.
        let r3 = store
            .retention_sweep_account(TEST_ACCOUNT, &p, NOW)
            .unwrap();
        assert_eq!(r3.junk_removed, 1);
        assert!(!r3.junk_capped);
    }

    #[test]
    fn trash_window_is_independent_of_junk() {
        let (_d, mds, store) = open_mailstore();
        let set = provision(&store);
        let junk = mk_container(&mds, &set, "Junk", Some("\\Junk"));
        let trash = mk_container(&mds, &set, "Trash", Some("\\Trash"));

        let junk_aged = add_at(&mds, &set, junk, "j", NOW - 10 * DAY_MS);
        let trash_aged = add_at(&mds, &set, trash, "t", NOW - 10 * DAY_MS);

        // Junk disabled (0), Trash armed at 7 days.
        let report = store
            .retention_sweep_account(TEST_ACCOUNT, &params(0, 7, false), NOW)
            .unwrap();
        assert_eq!(report.junk_removed, 0);
        assert_eq!(report.trash_removed, 1);
        assert!(
            membership_exists(&mds, &set, &junk_aged, &junk),
            "Junk untouched when junk_days=0"
        );
        assert!(
            !membership_exists(&mds, &set, &trash_aged, &trash),
            "Trash swept"
        );
    }

    #[test]
    fn disabled_policy_is_noop() {
        let (_d, mds, store) = open_mailstore();
        let set = provision(&store);
        let junk = mk_container(&mds, &set, "Junk", Some("\\Junk"));
        let aged = add_at(&mds, &set, junk, "aged", NOW - 100 * DAY_MS);

        let report = store
            .retention_sweep_account(TEST_ACCOUNT, &params(0, 0, false), NOW)
            .unwrap();
        assert_eq!(report, RetentionSweepReport::default());
        assert!(membership_exists(&mds, &set, &aged, &junk));
    }

    #[test]
    fn missing_junk_folder_is_noop() {
        // Account provisioned but has no \Junk mailbox → clean no-op.
        let (_d, _mds, store) = open_mailstore();
        let _set = provision(&store);
        let report = store
            .retention_sweep_account(TEST_ACCOUNT, &params(7, 7, false), NOW)
            .unwrap();
        assert_eq!(report, RetentionSweepReport::default());
    }

    #[test]
    fn unprovisioned_account_is_noop() {
        let (_d, _mds, store) = open_mailstore();
        // No ensure_account_set → no set.
        let report = store
            .retention_sweep_account(TEST_ACCOUNT, &params(7, 7, false), NOW)
            .unwrap();
        assert_eq!(report, RetentionSweepReport::default());
    }

    #[test]
    fn oversized_params_clamp_safely() {
        // The primitive must not trust its public caller: an absurd
        // window must NOT wrap the cutoff into the future (which would
        // delete everything), and an absurd cap must NOT disable the
        // guard. A 1-day-old item under a u64::MAX-day window survives.
        let (_d, mds, store) = open_mailstore();
        let set = provision(&store);
        let junk = mk_container(&mds, &set, "Junk", Some("\\Junk"));
        let fresh = add_at(&mds, &set, junk, "fresh", NOW - DAY_MS);

        let mut p = params(u64::MAX, 0, false);
        p.max_deletes_per_sweep = u64::MAX;
        let report = store
            .retention_sweep_account(TEST_ACCOUNT, &p, NOW)
            .unwrap();
        assert_eq!(
            report.junk_removed, 0,
            "huge window must not delete fresh mail"
        );
        assert!(membership_exists(&mds, &set, &fresh, &junk));
    }

    #[test]
    fn reaps_fts_when_last_membership_removed() {
        // Use create_email so the item carries a mail_search FTS row,
        // then age its Junk membership and sweep.
        let (_d, mds, store) = open_mailstore();
        let set = provision(&store);
        let junk = mk_container(&mds, &set, "Junk", Some("\\Junk"));

        let raw =
            b"From: a@example.com\r\nSubject: spam\r\nContent-Type: text/plain\r\n\r\nbody\r\n";
        let (hash, _) = put_blob(&mds, raw);
        let (id, _) = store
            .create_email(
                TEST_ACCOUNT,
                junk,
                hash,
                sample_envelope(),
                &[],
                Flags(0),
                Tags::new(),
                NOW,
            )
            .unwrap();
        assert_eq!(count_mail_search_rows_for_item(&mds, &set, &id), 1);
        age_membership(&mds, &set, &id, &junk, NOW - 30 * DAY_MS);

        let report = store
            .retention_sweep_account(TEST_ACCOUNT, &params(7, 0, false), NOW)
            .unwrap();
        assert_eq!(report.junk_removed, 1);
        assert!(
            mds.fetch_item_meta(&set, &id).is_err(),
            "orphaned item dropped"
        );
        assert_eq!(
            count_mail_search_rows_for_item(&mds, &set, &id),
            0,
            "FTS row reaped when the item is orphaned"
        );
    }

    #[test]
    fn preserves_fts_when_item_survives_elsewhere() {
        let (_d, mds, store) = open_mailstore();
        let set = provision(&store);
        let junk = mk_container(&mds, &set, "Junk", Some("\\Junk"));
        let inbox = mk_container(&mds, &set, "Inbox", Some("\\Inbox"));

        let raw =
            b"From: a@example.com\r\nSubject: keep\r\nContent-Type: text/plain\r\n\r\nbody\r\n";
        let (hash, _) = put_blob(&mds, raw);
        let (id, _) = store
            .create_email_with_memberships(
                TEST_ACCOUNT,
                &[junk, inbox],
                hash,
                sample_envelope(),
                &[],
                Flags(0),
                Tags::new(),
                NOW,
            )
            .unwrap();
        assert_eq!(count_mail_search_rows_for_item(&mds, &set, &id), 1);
        age_membership(&mds, &set, &id, &junk, NOW - 30 * DAY_MS);

        let report = store
            .retention_sweep_account(TEST_ACCOUNT, &params(7, 0, false), NOW)
            .unwrap();
        assert_eq!(report.junk_removed, 1);
        assert!(
            mds.fetch_item_meta(&set, &id).is_ok(),
            "item survives in Inbox"
        );
        assert_eq!(
            count_mail_search_rows_for_item(&mds, &set, &id),
            1,
            "FTS row PRESERVED while the item survives elsewhere"
        );
    }
}
