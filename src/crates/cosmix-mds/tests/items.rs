//! Phase 1b: item/membership/blob integration + invariants.

use cosmix_mds::{ContainerAttrs, Flags, Mds, Membership, SetId, SqliteCasMds};
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

fn unread() -> Flags {
    Flags(0)
}
fn seen() -> Flags {
    Flags(1)
}

#[test]
fn put_get_blob_roundtrip() {
    let (_d, mds) = open();
    let h = mds.put_blob(b"hello mds").unwrap();
    assert_eq!(mds.get_blob(&h).unwrap(), b"hello mds");
    assert_eq!(mds.blob_size(&h).unwrap(), 9);
    assert!(mds.blob_exists(&h).unwrap());
}

#[test]
fn add_item_into_one_container_increments_status() {
    let (_d, mds) = open();
    let s = fresh_set();
    mds.create_set(&s).unwrap();
    let inbox = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let blob = mds.put_blob(b"first message").unwrap();
    let report = mds
        .add_item(
            &s,
            &blob,
            &[Membership {
                container: inbox,
                flags: unread(),
                added_at: 100,
            }],
        )
        .unwrap();
    assert_eq!(report.placements.len(), 1);
    assert_eq!(report.placements[0].seq.0, 1);
    let st = mds.container_status(&s, &inbox).unwrap();
    assert_eq!(st.exists, 1);
    assert_eq!(st.unread, 1);
    assert_eq!(st.next_seq.0, 2);
    // change_seq starts at 1; one bump for the add → 2.
    assert_eq!(st.change_seq.0, 2);
}

#[test]
fn add_item_into_two_containers_is_atomic() {
    // Spec invariant 4: multi-row mutations either all commit or none.
    let (_d, mds) = open();
    let s = fresh_set();
    mds.create_set(&s).unwrap();
    let inbox = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let archive = mds.create_container(&s, None, "Archive", attrs()).unwrap();
    let blob = mds.put_blob(b"two-place delivery").unwrap();
    let report = mds
        .add_item(
            &s,
            &blob,
            &[
                Membership {
                    container: inbox,
                    flags: unread(),
                    added_at: 1,
                },
                Membership {
                    container: archive,
                    flags: seen(),
                    added_at: 1,
                },
            ],
        )
        .unwrap();
    assert_eq!(report.placements.len(), 2);
    let inbox_st = mds.container_status(&s, &inbox).unwrap();
    let arch_st = mds.container_status(&s, &archive).unwrap();
    assert_eq!(inbox_st.exists, 1);
    assert_eq!(inbox_st.unread, 1);
    assert_eq!(arch_st.exists, 1);
    assert_eq!(arch_st.unread, 0); // seen already
}

#[test]
fn add_item_with_unknown_container_rolls_everything_back() {
    let (_d, mds) = open();
    let s = fresh_set();
    mds.create_set(&s).unwrap();
    let inbox = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let bogus = cosmix_mds::ContainerId(uuid::Uuid::now_v7());
    let blob = mds.put_blob(b"will not stick").unwrap();
    let err = mds
        .add_item(
            &s,
            &blob,
            &[
                Membership {
                    container: inbox,
                    flags: unread(),
                    added_at: 0,
                },
                Membership {
                    container: bogus,
                    flags: unread(),
                    added_at: 0,
                },
            ],
        )
        .unwrap_err();
    assert!(format!("{err}").contains("not found"), "{err}");
    // Inbox must be untouched: nothing leaked from the failed tx.
    let st = mds.container_status(&s, &inbox).unwrap();
    assert_eq!(st.exists, 0);
    assert_eq!(st.unread, 0);
    assert_eq!(st.next_seq.0, 1);
    assert_eq!(st.change_seq.0, 1);
}

#[test]
fn seq_is_monotonic_and_never_reused() {
    // Spec invariant 1.
    let (_d, mds) = open();
    let s = fresh_set();
    mds.create_set(&s).unwrap();
    let c = mds.create_container(&s, None, "x", attrs()).unwrap();

    let mut seqs = Vec::new();
    for i in 0..5 {
        let h = mds.put_blob(format!("m{i}").as_bytes()).unwrap();
        let r = mds
            .add_item(
                &s,
                &h,
                &[Membership {
                    container: c,
                    flags: unread(),
                    added_at: i,
                }],
            )
            .unwrap();
        seqs.push((r.item_id, r.placements[0].seq.0));
    }
    // 1..=5
    let assigned: Vec<u32> = seqs.iter().map(|(_, s)| *s).collect();
    assert_eq!(assigned, vec![1, 2, 3, 4, 5]);

    // Remove the item with seq=3, then add another. Its seq must be
    // 6, not 3 — invariant 1 says freed seqs are gone forever.
    let (id3, _) = seqs[2];
    mds.remove_membership(&s, &id3, &c).unwrap();
    let h = mds.put_blob(b"after-removal").unwrap();
    let r = mds
        .add_item(
            &s,
            &h,
            &[Membership {
                container: c,
                flags: unread(),
                added_at: 99,
            }],
        )
        .unwrap();
    assert_eq!(r.placements[0].seq.0, 6);
}

#[test]
fn change_seq_strictly_increases_per_container() {
    // Spec invariant 2.
    let (_d, mds) = open();
    let s = fresh_set();
    mds.create_set(&s).unwrap();
    let c = mds.create_container(&s, None, "c", attrs()).unwrap();
    let h = mds.put_blob(b"x").unwrap();
    let r = mds
        .add_item(
            &s,
            &h,
            &[Membership {
                container: c,
                flags: unread(),
                added_at: 0,
            }],
        )
        .unwrap();
    let cs0 = r.placements[0].change_seq.0;
    let cs1 = mds.store_flags(&s, &r.item_id, &c, seen()).unwrap().0;
    let cs2 = mds.store_flags(&s, &r.item_id, &c, unread()).unwrap().0;
    assert!(cs0 < cs1);
    assert!(cs1 < cs2);
}

#[test]
fn flag_isolation_across_containers() {
    // Spec invariant 5.
    let (_d, mds) = open();
    let s = fresh_set();
    mds.create_set(&s).unwrap();
    let a = mds.create_container(&s, None, "A", attrs()).unwrap();
    let b = mds.create_container(&s, None, "B", attrs()).unwrap();
    let blob = mds.put_blob(b"shared").unwrap();
    let r = mds
        .add_item(
            &s,
            &blob,
            &[
                Membership {
                    container: a,
                    flags: unread(),
                    added_at: 0,
                },
                Membership {
                    container: b,
                    flags: unread(),
                    added_at: 0,
                },
            ],
        )
        .unwrap();
    // Mark seen in A only.
    mds.store_flags(&s, &r.item_id, &a, seen()).unwrap();
    let st_a = mds.container_status(&s, &a).unwrap();
    let st_b = mds.container_status(&s, &b).unwrap();
    assert_eq!(st_a.unread, 0);
    assert_eq!(st_b.unread, 1);
}

#[test]
fn copy_item_adds_membership_without_touching_source() {
    let (_d, mds) = open();
    let s = fresh_set();
    mds.create_set(&s).unwrap();
    let inbox = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let arch = mds.create_container(&s, None, "Archive", attrs()).unwrap();
    let blob = mds.put_blob(b"copy me").unwrap();
    let r = mds
        .add_item(
            &s,
            &blob,
            &[Membership {
                container: inbox,
                flags: unread(),
                added_at: 0,
            }],
        )
        .unwrap();
    let copy = mds.copy_item(&s, &r.item_id, &arch, seen()).unwrap();
    assert_eq!(copy.seq_dest.0, 1);
    let st_inbox = mds.container_status(&s, &inbox).unwrap();
    let st_arch = mds.container_status(&s, &arch).unwrap();
    assert_eq!(st_inbox.exists, 1);
    assert_eq!(st_inbox.unread, 1);
    assert_eq!(st_arch.exists, 1);
    // v1.1 §3 inheritance: dest adopts the source membership's flags;
    // caller's `seen()` is ignored when there's an existing membership
    // to inherit from. Source was `unread()`, so Archive is unread too.
    assert_eq!(st_arch.unread, 1);
}

#[test]
fn copy_into_existing_membership_is_error() {
    let (_d, mds) = open();
    let s = fresh_set();
    mds.create_set(&s).unwrap();
    let c = mds.create_container(&s, None, "x", attrs()).unwrap();
    let blob = mds.put_blob(b"once").unwrap();
    let r = mds
        .add_item(
            &s,
            &blob,
            &[Membership {
                container: c,
                flags: unread(),
                added_at: 0,
            }],
        )
        .unwrap();
    let err = mds.copy_item(&s, &r.item_id, &c, unread()).unwrap_err();
    assert!(format!("{err}").contains("already in container"), "{err}");
}

#[test]
fn move_item_loses_src_gains_dest_atomically() {
    let (_d, mds) = open();
    let s = fresh_set();
    mds.create_set(&s).unwrap();
    let inbox = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let arch = mds.create_container(&s, None, "Archive", attrs()).unwrap();
    let blob = mds.put_blob(b"will move").unwrap();
    let r = mds
        .add_item(
            &s,
            &blob,
            &[Membership {
                container: inbox,
                flags: unread(),
                added_at: 0,
            }],
        )
        .unwrap();
    let mv = mds
        .move_item(&s, &r.item_id, &inbox, &arch, seen())
        .unwrap();
    assert_eq!(mv.seq_dest.0, 1);
    let st_inbox = mds.container_status(&s, &inbox).unwrap();
    let st_arch = mds.container_status(&s, &arch).unwrap();
    assert_eq!(st_inbox.exists, 0);
    assert_eq!(st_inbox.unread, 0);
    assert_eq!(st_arch.exists, 1);
    // v1.1 §3 inheritance: dest adopts src's flags, ignoring caller's
    // `seen()`. Source was `unread()`, so Archive remains unread.
    assert_eq!(st_arch.unread, 1);
}

#[test]
fn remove_membership_decrements_counts() {
    let (_d, mds) = open();
    let s = fresh_set();
    mds.create_set(&s).unwrap();
    let c = mds.create_container(&s, None, "x", attrs()).unwrap();
    let blob = mds.put_blob(b"to remove").unwrap();
    let r = mds
        .add_item(
            &s,
            &blob,
            &[Membership {
                container: c,
                flags: unread(),
                added_at: 0,
            }],
        )
        .unwrap();
    mds.remove_membership(&s, &r.item_id, &c).unwrap();
    let st = mds.container_status(&s, &c).unwrap();
    assert_eq!(st.exists, 0);
    assert_eq!(st.unread, 0);
}

#[test]
fn store_flags_on_unknown_membership_errors() {
    let (_d, mds) = open();
    let s = fresh_set();
    mds.create_set(&s).unwrap();
    let c = mds.create_container(&s, None, "x", attrs()).unwrap();
    let bogus = cosmix_mds::ItemId(uuid::Uuid::now_v7());
    let err = mds.store_flags(&s, &bogus, &c, seen()).unwrap_err();
    assert!(format!("{err}").contains("not present"), "{err}");
}

#[test]
fn move_into_self_is_error() {
    let (_d, mds) = open();
    let s = fresh_set();
    mds.create_set(&s).unwrap();
    let c = mds.create_container(&s, None, "x", attrs()).unwrap();
    let blob = mds.put_blob(b"x").unwrap();
    let r = mds
        .add_item(
            &s,
            &blob,
            &[Membership {
                container: c,
                flags: unread(),
                added_at: 0,
            }],
        )
        .unwrap();
    let err = mds.move_item(&s, &r.item_id, &c, &c, unread()).unwrap_err();
    assert!(format!("{err}").contains("differ"), "{err}");
}

#[test]
fn deduplicates_blob_for_identical_messages() {
    // Two add_item calls with the same blob hash share one CAS file
    // and produce two distinct items.
    let (_d, mds) = open();
    let s = fresh_set();
    mds.create_set(&s).unwrap();
    let c = mds.create_container(&s, None, "x", attrs()).unwrap();
    let h1 = mds.put_blob(b"identical body").unwrap();
    let h2 = mds.put_blob(b"identical body").unwrap();
    assert_eq!(h1, h2);
    let r1 = mds
        .add_item(
            &s,
            &h1,
            &[Membership {
                container: c,
                flags: unread(),
                added_at: 1,
            }],
        )
        .unwrap();
    let r2 = mds
        .add_item(
            &s,
            &h2,
            &[Membership {
                container: c,
                flags: unread(),
                added_at: 2,
            }],
        )
        .unwrap();
    assert_ne!(r1.item_id, r2.item_id);
    let st = mds.container_status(&s, &c).unwrap();
    assert_eq!(st.exists, 2);
}
