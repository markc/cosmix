//! Phase 8c — `set_change` emission on every mutation path +
//! `changes_since_set` cursor semantics + `item_memberships` round-trip.
//!
//! The acceptance bar for 8c (per Mark's Option-A scope sign-off):
//!
//!   * Every successful mutation that writes through the centralized
//!     `allocate_set_change` helper emits *exactly* the `set_change`
//!     row(s) the spec promises — the right `kind`, the right count,
//!     the right `(container_id, item_id)` pair. Partial backing of
//!     `changes_since_set` would silently desync JMAP `Email/changes`
//!     consumers, so each of the six paths is asserted independently.
//!   * `changes_since_set` returns rows in `set_change_seq` order with
//!     a `LIMIT N+1` cursor: page boundary returns the last *returned*
//!     row's seq as `Some(next)`; tip returns `None`.
//!   * `item_memberships` round-trips `(container, flags, tags)`
//!     ordered by `container_id`, including empty-tag and multi-tag
//!     memberships.
//!   * `ItemMoved` is delivered to subscribers of BOTH src and dest in
//!     addition to the existing `ItemRemoved` / `ItemAdded` (additive,
//!     not a replacement — IMAP IDLE consumers must keep working).

use cosmix_mds::{
    ChangeKind, ContainerAttrs, ContainerEvent, Flags, Mds, Membership, SetChangeToken, SetId,
    SqliteCasMds, Tags,
};
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tokio::time::timeout;

const DEADLINE: Duration = Duration::from_secs(5);

fn attrs() -> ContainerAttrs {
    ContainerAttrs {
        special_use: None,
        subscribed: true,
        extra: serde_json::json!({}),
    }
}

fn open_sync() -> (TempDir, SqliteCasMds) {
    let d = TempDir::new().unwrap();
    let mds = SqliteCasMds::open(d.path()).unwrap();
    (d, mds)
}

fn open_arc() -> (TempDir, Arc<SqliteCasMds>) {
    let d = TempDir::new().unwrap();
    let mds = Arc::new(SqliteCasMds::open(d.path()).unwrap());
    (d, mds)
}

fn fresh_set(mds: &SqliteCasMds) -> SetId {
    let s = SetId(uuid::Uuid::now_v7());
    mds.create_set(&s).unwrap();
    s
}

/// Drain the entire `set_change` stream from genesis. The "0" token
/// is *exclusive* (we want everything strictly after the start), and a
/// generous limit lets the test verify exact row counts without
/// caring about pagination here.
fn all_changes(mds: &SqliteCasMds, set: &SetId) -> Vec<cosmix_mds::SetChange> {
    let (rows, next) = mds
        .changes_since_set(set, SetChangeToken(0), 1024)
        .expect("changes_since_set");
    assert!(
        next.is_none(),
        "test bug: drained 1024 rows and stream still extends — bump the limit"
    );
    rows
}

// ---------------------------------------------------------------------
// Per-path emission tests — one per mutation surface.
// Each test asserts: (a) exact row count, (b) kind, (c) container + item.
// Together these cover the six paths Mark called out:
//   add_item, copy_item, move_item, remove_membership, store_flags,
//   store_membership_keywords.
// ---------------------------------------------------------------------

#[test]
fn add_item_emits_one_added_per_placement() {
    let (_d, mds) = open_sync();
    let s = fresh_set(&mds);
    let inbox = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let arch = mds.create_container(&s, None, "Archive", attrs()).unwrap();
    let blob = mds.put_blob(b"multi-placement").unwrap();

    let report = mds
        .add_item(
            &s,
            &blob,
            &[
                Membership {
                    container: inbox,
                    flags: Flags(0),
                    added_at: 1,
                },
                Membership {
                    container: arch,
                    flags: Flags(0),
                    added_at: 1,
                },
            ],
        )
        .unwrap();

    let rows = all_changes(&mds, &s);
    assert_eq!(rows.len(), 2, "one set_change per placement");
    for r in &rows {
        assert_eq!(r.kind, ChangeKind::Added);
        assert_eq!(r.item_id, report.item_id);
    }
    let containers: std::collections::HashSet<_> = rows.iter().map(|r| r.container_id).collect();
    assert!(containers.contains(&inbox));
    assert!(containers.contains(&arch));
    // set_change_seq strictly monotonic and starts at 1 for a fresh set.
    assert_eq!(rows[0].set_change_seq, SetChangeToken(1));
    assert_eq!(rows[1].set_change_seq, SetChangeToken(2));
}

#[test]
fn copy_item_emits_one_added_at_dest() {
    let (_d, mds) = open_sync();
    let s = fresh_set(&mds);
    let inbox = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let arch = mds.create_container(&s, None, "Archive", attrs()).unwrap();
    let blob = mds.put_blob(b"copy me").unwrap();
    let r = mds
        .add_item(
            &s,
            &blob,
            &[Membership {
                container: inbox,
                flags: Flags(0),
                added_at: 1,
            }],
        )
        .unwrap();
    let item_id = r.item_id;

    mds.copy_item(&s, &item_id, &arch, Flags(0)).unwrap();

    let rows = all_changes(&mds, &s);
    assert_eq!(
        rows.len(),
        2,
        "one Added per add_item placement, one for copy"
    );
    let copy_row = rows.last().unwrap();
    assert_eq!(copy_row.kind, ChangeKind::Added);
    assert_eq!(copy_row.container_id, arch);
    assert_eq!(copy_row.item_id, item_id);
}

#[test]
fn move_item_emits_removed_and_added_atomically() {
    let (_d, mds) = open_sync();
    let s = fresh_set(&mds);
    let inbox = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let arch = mds.create_container(&s, None, "Archive", attrs()).unwrap();
    let blob = mds.put_blob(b"travel").unwrap();
    let r = mds
        .add_item(
            &s,
            &blob,
            &[Membership {
                container: inbox,
                flags: Flags(0),
                added_at: 1,
            }],
        )
        .unwrap();
    let item_id = r.item_id;

    mds.move_item(&s, &item_id, &inbox, &arch, Flags(0))
        .unwrap();

    let rows = all_changes(&mds, &s);
    // 1 add_item Added + (1 Removed src + 1 Added dest) from move = 3.
    assert_eq!(rows.len(), 3);
    let removed = rows.iter().find(|r| r.kind == ChangeKind::Removed).unwrap();
    let added_dest = rows
        .iter()
        .filter(|r| r.kind == ChangeKind::Added)
        .find(|r| r.container_id == arch)
        .unwrap();
    assert_eq!(removed.container_id, inbox);
    assert_eq!(removed.item_id, item_id);
    assert_eq!(added_dest.item_id, item_id);
    // The pair is allocated inside the same tx: their set_change_seq
    // values must be consecutive (no other writer between them).
    assert_eq!(
        added_dest.set_change_seq.0,
        removed.set_change_seq.0 + 1,
        "move pair must allocate adjacent set_change_seq"
    );
}

#[test]
fn remove_membership_emits_one_removed() {
    let (_d, mds) = open_sync();
    let s = fresh_set(&mds);
    let inbox = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let arch = mds.create_container(&s, None, "Archive", attrs()).unwrap();
    let blob = mds.put_blob(b"two homes").unwrap();
    let r = mds
        .add_item(
            &s,
            &blob,
            &[
                Membership {
                    container: inbox,
                    flags: Flags(0),
                    added_at: 1,
                },
                Membership {
                    container: arch,
                    flags: Flags(0),
                    added_at: 1,
                },
            ],
        )
        .unwrap();
    let item_id = r.item_id;

    mds.remove_membership(&s, &item_id, &inbox).unwrap();

    let rows = all_changes(&mds, &s);
    // 2 from add_item + 1 from remove = 3.
    assert_eq!(rows.len(), 3);
    let removed = rows.last().unwrap();
    assert_eq!(removed.kind, ChangeKind::Removed);
    assert_eq!(removed.container_id, inbox);
    assert_eq!(removed.item_id, item_id);
}

#[test]
fn store_flags_emits_one_metadata_changed() {
    let (_d, mds) = open_sync();
    let s = fresh_set(&mds);
    let inbox = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let blob = mds.put_blob(b"flag me").unwrap();
    let r = mds
        .add_item(
            &s,
            &blob,
            &[Membership {
                container: inbox,
                flags: Flags(0),
                added_at: 1,
            }],
        )
        .unwrap();
    let item_id = r.item_id;

    mds.store_flags(&s, &item_id, &inbox, Flags(1)).unwrap();

    let rows = all_changes(&mds, &s);
    assert_eq!(rows.len(), 2, "1 Added + 1 MetadataChanged");
    let meta = rows.last().unwrap();
    assert_eq!(meta.kind, ChangeKind::MetadataChanged);
    assert_eq!(meta.container_id, inbox);
    assert_eq!(meta.item_id, item_id);
}

#[test]
fn store_membership_keywords_emits_one_metadata_changed() {
    let (_d, mds) = open_sync();
    let s = fresh_set(&mds);
    let inbox = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let blob = mds.put_blob(b"keyword me").unwrap();
    let r = mds
        .add_item(
            &s,
            &blob,
            &[Membership {
                container: inbox,
                flags: Flags(0),
                added_at: 1,
            }],
        )
        .unwrap();
    let item_id = r.item_id;

    let tags: Tags = ["work".to_string(), "urgent".to_string()]
        .into_iter()
        .collect();
    mds.store_membership_keywords(&s, &item_id, &inbox, Flags(1), tags.clone())
        .unwrap();

    let rows = all_changes(&mds, &s);
    assert_eq!(rows.len(), 2, "1 Added + 1 MetadataChanged from keywords");
    let meta = rows.last().unwrap();
    assert_eq!(meta.kind, ChangeKind::MetadataChanged);
    assert_eq!(meta.container_id, inbox);
    assert_eq!(meta.item_id, item_id);
}

// ---------------------------------------------------------------------
// changes_since_set cursor semantics — limit, pagination, tip.
// ---------------------------------------------------------------------

#[test]
fn changes_since_set_paginates_with_n_plus_one_probe() {
    let (_d, mds) = open_sync();
    let s = fresh_set(&mds);
    let inbox = mds.create_container(&s, None, "INBOX", attrs()).unwrap();

    // 5 ADDED rows, set_change_seq 1..=5.
    let mut item_ids = Vec::new();
    for i in 0..5 {
        let blob = mds.put_blob(format!("item-{i}").as_bytes()).unwrap();
        let r = mds
            .add_item(
                &s,
                &blob,
                &[Membership {
                    container: inbox,
                    flags: Flags(0),
                    added_at: i as i64,
                }],
            )
            .unwrap();
        item_ids.push(r.item_id);
    }

    // Page 1: limit 2 starting from 0 returns rows 1,2 with cursor=2.
    let (page1, cur1) = mds.changes_since_set(&s, SetChangeToken(0), 2).unwrap();
    assert_eq!(page1.len(), 2);
    assert_eq!(page1[0].set_change_seq, SetChangeToken(1));
    assert_eq!(page1[1].set_change_seq, SetChangeToken(2));
    assert_eq!(
        cur1,
        Some(SetChangeToken(2)),
        "cursor = last RETURNED row's seq"
    );

    // Page 2: from cursor 2, limit 2 returns rows 3,4 with cursor=4.
    let (page2, cur2) = mds.changes_since_set(&s, cur1.unwrap(), 2).unwrap();
    assert_eq!(page2.len(), 2);
    assert_eq!(page2[0].set_change_seq, SetChangeToken(3));
    assert_eq!(page2[1].set_change_seq, SetChangeToken(4));
    assert_eq!(cur2, Some(SetChangeToken(4)));

    // Page 3 (tip): from cursor 4, limit 2 returns row 5 + None.
    let (page3, cur3) = mds.changes_since_set(&s, cur2.unwrap(), 2).unwrap();
    assert_eq!(page3.len(), 1);
    assert_eq!(page3[0].set_change_seq, SetChangeToken(5));
    assert_eq!(cur3, None, "at tip — no more pages");

    // Past tip: empty page, None cursor.
    let (page4, cur4) = mds.changes_since_set(&s, SetChangeToken(5), 2).unwrap();
    assert!(page4.is_empty());
    assert!(cur4.is_none());
}

#[test]
fn changes_since_set_limit_zero_returns_empty_no_cursor() {
    let (_d, mds) = open_sync();
    let s = fresh_set(&mds);
    let inbox = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let blob = mds.put_blob(b"x").unwrap();
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

    let (rows, cur) = mds.changes_since_set(&s, SetChangeToken(0), 0).unwrap();
    assert!(rows.is_empty(), "limit 0 returns empty page");
    assert!(cur.is_none(), "limit 0 returns no cursor");
}

#[test]
fn changes_since_set_empty_when_set_has_no_mutations() {
    let (_d, mds) = open_sync();
    let s = fresh_set(&mds);
    // No mutations after create_set.
    let (rows, cur) = mds.changes_since_set(&s, SetChangeToken(0), 16).unwrap();
    assert!(rows.is_empty());
    assert!(cur.is_none());
}

// ---------------------------------------------------------------------
// item_memberships round-trip — covers the read side of v1.1 keywords.
// ---------------------------------------------------------------------

#[test]
fn item_memberships_returns_per_membership_flags_and_tags() {
    let (_d, mds) = open_sync();
    let s = fresh_set(&mds);
    let inbox = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let arch = mds.create_container(&s, None, "Archive", attrs()).unwrap();
    let blob = mds.put_blob(b"cross-mailbox").unwrap();
    let r = mds
        .add_item(
            &s,
            &blob,
            &[
                Membership {
                    container: inbox,
                    flags: Flags(0),
                    added_at: 1,
                },
                Membership {
                    container: arch,
                    flags: Flags(0),
                    added_at: 1,
                },
            ],
        )
        .unwrap();
    let item_id = r.item_id;

    // Per-membership keyword DIVERGENCE is exercised here via direct
    // `store_membership_keywords` (the IMAP STORE per-mailbox path,
    // spec §3 lines 342-344). JMAP `Email/set keywords` does NOT
    // produce divergent state — it fans out to all memberships and
    // they agree by invariant. The bulk JMAP path lives in
    // `store_item_keywords` (see `bulk_keyword_*` tests below).
    let inbox_tags: Tags = ["urgent".to_string()].into_iter().collect();
    let arch_tags: Tags = ["filed".to_string(), "work".to_string()]
        .into_iter()
        .collect();
    mds.store_membership_keywords(&s, &item_id, &inbox, Flags(1), inbox_tags.clone())
        .unwrap();
    mds.store_membership_keywords(&s, &item_id, &arch, Flags(0), arch_tags.clone())
        .unwrap();

    let got = mds.item_memberships(&s, &item_id).unwrap();
    assert_eq!(got.len(), 2);

    // Find each by container_id rather than relying on UUID order.
    let inbox_row = got.iter().find(|(c, _, _)| *c == inbox).unwrap();
    let arch_row = got.iter().find(|(c, _, _)| *c == arch).unwrap();
    assert_eq!(inbox_row.1, Flags(1));
    assert_eq!(inbox_row.2, inbox_tags);
    assert_eq!(arch_row.1, Flags(0));
    assert_eq!(arch_row.2, arch_tags);
}

#[test]
fn copy_item_inherits_flags_and_tags_from_existing_membership() {
    let (_d, mds) = open_sync();
    let s = fresh_set(&mds);
    let inbox = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let arch = mds.create_container(&s, None, "Archive", attrs()).unwrap();
    let blob = mds.put_blob(b"keyword inherit").unwrap();
    let r = mds
        .add_item(
            &s,
            &blob,
            &[Membership {
                container: inbox,
                flags: Flags(0),
                added_at: 1,
            }],
        )
        .unwrap();
    let item_id = r.item_id;

    // Establish the agreed keyword state on the source membership
    // before copying.
    let want_tags: Tags = ["pinned".to_string(), "work".to_string()]
        .into_iter()
        .collect();
    mds.store_membership_keywords(&s, &item_id, &inbox, Flags(1), want_tags.clone())
        .unwrap();

    // Caller passes Flags(99) — the inheritance rule must override it,
    // so the dest membership ends up with the source's (flags, tags),
    // not the caller's flags or empty tags.
    mds.copy_item(&s, &item_id, &arch, Flags(99)).unwrap();

    let memberships = mds.item_memberships(&s, &item_id).unwrap();
    let arch_row = memberships
        .iter()
        .find(|(c, _, _)| *c == arch)
        .expect("dest membership exists");
    assert_eq!(
        arch_row.1,
        Flags(1),
        "copy must inherit src flags, ignoring caller-supplied"
    );
    assert_eq!(arch_row.2, want_tags, "copy must inherit src tags");
}

#[test]
fn move_item_inherits_flags_and_tags_from_src_membership() {
    let (_d, mds) = open_sync();
    let s = fresh_set(&mds);
    let inbox = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let arch = mds.create_container(&s, None, "Archive", attrs()).unwrap();
    let blob = mds.put_blob(b"keyword inherit move").unwrap();
    let r = mds
        .add_item(
            &s,
            &blob,
            &[Membership {
                container: inbox,
                flags: Flags(0),
                added_at: 1,
            }],
        )
        .unwrap();
    let item_id = r.item_id;

    let want_tags: Tags = ["urgent".to_string()].into_iter().collect();
    mds.store_membership_keywords(&s, &item_id, &inbox, Flags(1), want_tags.clone())
        .unwrap();

    mds.move_item(&s, &item_id, &inbox, &arch, Flags(99))
        .unwrap();

    let memberships = mds.item_memberships(&s, &item_id).unwrap();
    assert_eq!(memberships.len(), 1, "move drops src, leaves only dest");
    let (cid, fl, tg) = &memberships[0];
    assert_eq!(*cid, arch);
    assert_eq!(*fl, Flags(1), "move dest must inherit src flags");
    assert_eq!(*tg, want_tags, "move dest must inherit src tags");
}

#[test]
fn item_memberships_empty_for_unknown_item() {
    let (_d, mds) = open_sync();
    let s = fresh_set(&mds);
    let bogus = cosmix_mds::ItemId(uuid::Uuid::now_v7());
    let got = mds.item_memberships(&s, &bogus).unwrap();
    assert!(got.is_empty(), "unknown item returns empty, not an error");
}

// ---------------------------------------------------------------------
// ItemMoved fan-out — additive, not a replacement.
// ---------------------------------------------------------------------

#[tokio::test]
async fn move_publishes_item_moved_to_both_src_and_dest() {
    let (_d, mds) = open_arc();
    let s = SetId(uuid::Uuid::now_v7());
    mds.create_set(&s).unwrap();
    let inbox = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let arch = mds.create_container(&s, None, "Archive", attrs()).unwrap();
    let blob = mds.put_blob(b"moving").unwrap();
    let r = mds
        .add_item(
            &s,
            &blob,
            &[Membership {
                container: inbox,
                flags: Flags(0),
                added_at: 1,
            }],
        )
        .unwrap();
    let item_id = r.item_id;

    let mut rx_src = mds.subscribe(&s, &inbox);
    let mut rx_dst = mds.subscribe(&s, &arch);

    let mds2 = mds.clone();
    let writer = tokio::task::spawn_blocking(move || {
        mds2.move_item(&s, &item_id, &inbox, &arch, Flags(0))
            .unwrap()
    });

    // Per container.rs publish order: src sees ItemRemoved then
    // ItemMoved; dest sees ItemAdded then ItemMoved. The first event
    // is the additive guarantee (proven in tests/notifier.rs); the
    // *second* event on each channel is what 8c added.
    let src_first = timeout(DEADLINE, rx_src.recv()).await.unwrap().unwrap();
    let src_second = timeout(DEADLINE, rx_src.recv()).await.unwrap().unwrap();
    let dst_first = timeout(DEADLINE, rx_dst.recv()).await.unwrap().unwrap();
    let dst_second = timeout(DEADLINE, rx_dst.recv()).await.unwrap().unwrap();
    let _ = writer.await.unwrap();

    assert!(
        matches!(src_first, ContainerEvent::ItemRemoved { item_id: id, .. } if id == item_id),
        "src first event must remain ItemRemoved (additive contract)"
    );
    assert!(
        matches!(dst_first, ContainerEvent::ItemAdded { item_id: id, .. } if id == item_id),
        "dst first event must remain ItemAdded (additive contract)"
    );

    for (label, ev) in [("src", src_second), ("dst", dst_second)] {
        match ev {
            ContainerEvent::ItemMoved {
                item_id: id,
                src,
                dest,
                change_seq_src,
                change_seq_dest,
                set_change_seq_src,
                set_change_seq_dest,
            } => {
                assert_eq!(id, item_id, "{label}: ItemMoved item_id");
                assert_eq!(src, inbox, "{label}: ItemMoved src");
                assert_eq!(dest, arch, "{label}: ItemMoved dest");
                // Set-change tokens were allocated inside one tx — adjacent.
                assert_eq!(
                    set_change_seq_dest.0,
                    set_change_seq_src.0 + 1,
                    "{label}: set_change_seq pair must be adjacent"
                );
                // Per-container change_seq is independent per container —
                // both should be > 0 (the bump is what subscribe_wakes_on_*
                // tests exercise).
                assert!(change_seq_src.0 > 0);
                assert!(change_seq_dest.0 > 0);
            }
            other => panic!("{label}: expected ItemMoved second, got {other:?}"),
        }
    }
}

// ---------------------------------------------------------------------
// store_item_keywords — JMAP `Email/set keywords` fan-out path.
// Atomic across all current memberships; one set_change row per
// affected membership; per-container FlagsChanged notifier event.
// ---------------------------------------------------------------------

#[test]
fn bulk_keyword_writes_to_all_memberships_atomically() {
    let (_d, mds) = open_sync();
    let s = fresh_set(&mds);
    let inbox = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let arch = mds.create_container(&s, None, "Archive", attrs()).unwrap();
    let sent = mds.create_container(&s, None, "Sent", attrs()).unwrap();
    let blob = mds.put_blob(b"three placements").unwrap();
    let r = mds
        .add_item(
            &s,
            &blob,
            &[
                Membership {
                    container: inbox,
                    flags: Flags(0),
                    added_at: 1,
                },
                Membership {
                    container: arch,
                    flags: Flags(0),
                    added_at: 1,
                },
                Membership {
                    container: sent,
                    flags: Flags(0),
                    added_at: 1,
                },
            ],
        )
        .unwrap();
    let item_id = r.item_id;

    let want_tags: Tags = ["pinned".to_string(), "work".to_string()]
        .into_iter()
        .collect();
    let updated = mds
        .store_item_keywords(&s, &item_id, Flags(1), want_tags.clone())
        .unwrap();
    assert_eq!(updated.len(), 3, "one tuple per affected membership");

    // All three memberships agree on flags + tags — the JMAP invariant.
    let memberships = mds.item_memberships(&s, &item_id).unwrap();
    assert_eq!(memberships.len(), 3);
    for (_, fl, tg) in &memberships {
        assert_eq!(*fl, Flags(1));
        assert_eq!(*tg, want_tags);
    }
}

#[test]
fn bulk_keyword_emits_one_set_change_per_membership() {
    let (_d, mds) = open_sync();
    let s = fresh_set(&mds);
    let inbox = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let arch = mds.create_container(&s, None, "Archive", attrs()).unwrap();
    let blob = mds.put_blob(b"two placements").unwrap();
    let r = mds
        .add_item(
            &s,
            &blob,
            &[
                Membership {
                    container: inbox,
                    flags: Flags(0),
                    added_at: 1,
                },
                Membership {
                    container: arch,
                    flags: Flags(0),
                    added_at: 1,
                },
            ],
        )
        .unwrap();
    let item_id = r.item_id;

    let tags: Tags = ["x".to_string()].into_iter().collect();
    mds.store_item_keywords(&s, &item_id, Flags(1), tags)
        .unwrap();

    let rows = all_changes(&mds, &s);
    // 2 ADDED from add_item + 2 METADATA_CHANGED from the fan-out.
    assert_eq!(rows.len(), 4);
    let meta: Vec<_> = rows
        .iter()
        .filter(|r| r.kind == ChangeKind::MetadataChanged)
        .collect();
    assert_eq!(
        meta.len(),
        2,
        "one METADATA_CHANGED per affected membership"
    );
    for m in &meta {
        assert_eq!(m.item_id, item_id);
    }
    let containers: std::collections::HashSet<_> = meta.iter().map(|r| r.container_id).collect();
    assert!(containers.contains(&inbox));
    assert!(containers.contains(&arch));
    // Both METADATA_CHANGED rows allocated in one tx → adjacent
    // set_change_seq.
    assert_eq!(meta[1].set_change_seq.0, meta[0].set_change_seq.0 + 1);
}

#[tokio::test]
async fn bulk_keyword_publishes_flags_changed_per_container() {
    let (_d, mds) = open_arc();
    let s = SetId(uuid::Uuid::now_v7());
    mds.create_set(&s).unwrap();
    let inbox = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let arch = mds.create_container(&s, None, "Archive", attrs()).unwrap();
    let blob = mds.put_blob(b"notify both").unwrap();
    let r = mds
        .add_item(
            &s,
            &blob,
            &[
                Membership {
                    container: inbox,
                    flags: Flags(0),
                    added_at: 1,
                },
                Membership {
                    container: arch,
                    flags: Flags(0),
                    added_at: 1,
                },
            ],
        )
        .unwrap();
    let item_id = r.item_id;

    let mut rx_inbox = mds.subscribe(&s, &inbox);
    let mut rx_arch = mds.subscribe(&s, &arch);

    let mds2 = mds.clone();
    let writer = tokio::task::spawn_blocking(move || {
        let tags: Tags = ["fanout".to_string()].into_iter().collect();
        mds2.store_item_keywords(&s, &item_id, Flags(1), tags)
            .unwrap()
    });

    let inbox_ev = timeout(DEADLINE, rx_inbox.recv()).await.unwrap().unwrap();
    let arch_ev = timeout(DEADLINE, rx_arch.recv()).await.unwrap().unwrap();
    let _ = writer.await.unwrap();

    for (label, ev) in [("inbox", inbox_ev), ("arch", arch_ev)] {
        match ev {
            ContainerEvent::FlagsChanged { item_id: id, .. } => {
                assert_eq!(id, item_id, "{label}: FlagsChanged item_id");
            }
            other => panic!("{label}: expected FlagsChanged, got {other:?}"),
        }
    }
}

#[test]
fn bulk_keyword_no_op_for_unknown_item() {
    let (_d, mds) = open_sync();
    let s = fresh_set(&mds);
    let _ = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let bogus = cosmix_mds::ItemId(uuid::Uuid::now_v7());

    let updated = mds
        .store_item_keywords(&s, &bogus, Flags(1), Tags::default())
        .unwrap();
    assert!(updated.is_empty(), "unknown item → empty Vec, not an error");

    // Critically: no transaction committed → no set_change row written.
    // (A naïve impl that always allocates a set_change_seq before the
    // membership scan would leak a phantom row here.)
    let rows = all_changes(&mds, &s);
    assert!(rows.is_empty(), "no-op must not advance set_change stream");
}
