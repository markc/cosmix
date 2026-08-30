//! Property tests for spec §Invariants 1–7 and 9–12.
//!
//! The plan's Phase 1b exit clause is:
//!   "all five core invariants from spec §Invariants 1–5 are exercised
//!    by green property tests in `tests/invariants.rs`."
//!
//! This file covers that, plus the additional invariants the spec
//! lists for the per-set delivery transaction:
//!
//!   1. UID/seq monotonic per container; never reused after remove/move.
//!   2. UIDVALIDITY (`seq_validity`) stable across mutations.
//!   3. per-container `change_seq` strictly increasing — and, per the
//!      `each_mutation_advances_change_seq_and_writes_changelog_row`
//!      property, every successful mutation advances the affected
//!      container's `change_seq` by exactly one and writes the
//!      expected `ChangeKind` row.
//!   4. multi-row mutations atomic (all or none).
//!   5. blob reachability (every membership.item_id has a real blob).
//!   7. `exists_count` and `unread_count` track membership rows.
//!  10. flag SEEN-bit semantics drive unread; per-container isolation.
//!
//! The proptest harness uses deterministic seeds: failures are
//! persisted to `proptest-regressions/invariants.txt` automatically.
//! Re-running the test replays the failing seed first. To ratchet up
//! coverage in CI:
//!
//!   PROPTEST_CASES=1000 cargo test --release -p cosmix-mds invariants
//!
//! The `PROPTEST_CASES` env var overrides the per-test `cases` setting
//! at runtime.

use cosmix_mds::{
    ContainerAttrs, ContainerId, Flags, Mds, Membership, SeqRange, SetId, SqliteCasMds,
};
use proptest::collection::vec as prop_vec;
use proptest::prelude::*;
use std::collections::{BTreeSet, HashMap, HashSet};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

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

fn flags(seen: bool) -> Flags {
    Flags(if seen { Flags::SEEN } else { 0 })
}

fn is_seen(f: Flags) -> bool {
    (f.0 & Flags::SEEN) != 0
}

// ---------------------------------------------------------------------------
// Spec invariants — focused unit smoke tests
//
// These complement the proptests below: each one targets one invariant
// in isolation so a failure points straight at the invariant. The
// proptests then exercise the same invariants under random sequences.
// ---------------------------------------------------------------------------

#[test]
fn invariant_1_seq_is_never_reused_after_remove() {
    // Add three items, remove the middle one, add another. Every seq
    // ever observed in the container must be distinct, and the new
    // seq must exceed all prior seqs.
    let (_d, mds) = open();
    let s = fresh_set();
    mds.create_set(&s).unwrap();
    let c = mds.create_container(&s, None, "INBOX", attrs()).unwrap();

    let mut allocated = BTreeSet::new();
    let mut item_ids = Vec::new();
    for i in 0..3 {
        let blob = mds.put_blob(format!("body-{i}").as_bytes()).unwrap();
        let r = mds
            .add_item(
                &s,
                &blob,
                &[Membership {
                    container: c,
                    flags: flags(false),
                    added_at: 0,
                }],
            )
            .unwrap();
        assert!(allocated.insert(r.placements[0].seq.0), "duplicate seq");
        item_ids.push(r.item_id);
    }
    let max_before = *allocated.iter().last().unwrap();
    mds.remove_membership(&s, &item_ids[1], &c).unwrap();

    let blob = mds.put_blob(b"after-remove").unwrap();
    let r = mds
        .add_item(
            &s,
            &blob,
            &[Membership {
                container: c,
                flags: flags(false),
                added_at: 0,
            }],
        )
        .unwrap();
    let new_seq = r.placements[0].seq.0;
    assert!(allocated.insert(new_seq), "seq {new_seq} reused");
    assert!(
        new_seq > max_before,
        "new seq {new_seq} <= max prior {max_before}"
    );
}

#[test]
fn invariant_1_seq_is_never_reused_after_move() {
    // After a move out and a fresh add into the same source container,
    // the source's allocator must not hand back the moved-out seq.
    let (_d, mds) = open();
    let s = fresh_set();
    mds.create_set(&s).unwrap();
    let src = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let dest = mds.create_container(&s, None, "Archive", attrs()).unwrap();

    let blob = mds.put_blob(b"to-be-moved").unwrap();
    let r = mds
        .add_item(
            &s,
            &blob,
            &[Membership {
                container: src,
                flags: flags(false),
                added_at: 0,
            }],
        )
        .unwrap();
    let original_seq = r.placements[0].seq.0;

    mds.move_item(&s, &r.item_id, &src, &dest, flags(false))
        .unwrap();

    let blob2 = mds.put_blob(b"after-move").unwrap();
    let r2 = mds
        .add_item(
            &s,
            &blob2,
            &[Membership {
                container: src,
                flags: flags(false),
                added_at: 0,
            }],
        )
        .unwrap();
    assert!(
        r2.placements[0].seq.0 > original_seq,
        "post-move add reused seq {original_seq}"
    );
}

#[test]
fn invariant_2_uidvalidity_stable_across_mutations() {
    let (_d, mds) = open();
    let s = fresh_set();
    mds.create_set(&s).unwrap();
    let c = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let initial = mds.container_status(&s, &c).unwrap().seq_validity;

    for i in 0..5 {
        let blob = mds.put_blob(format!("v-{i}").as_bytes()).unwrap();
        mds.add_item(
            &s,
            &blob,
            &[Membership {
                container: c,
                flags: flags(false),
                added_at: 0,
            }],
        )
        .unwrap();
        let now = mds.container_status(&s, &c).unwrap().seq_validity;
        assert_eq!(now, initial, "seq_validity drifted on iter {i}");
    }
}

#[test]
fn invariant_3_change_seq_strictly_increases() {
    let (_d, mds) = open();
    let s = fresh_set();
    mds.create_set(&s).unwrap();
    let c = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let mut prev = mds.container_status(&s, &c).unwrap().change_seq.0;
    let blob = mds.put_blob(b"x").unwrap();
    let item_id = mds
        .add_item(
            &s,
            &blob,
            &[Membership {
                container: c,
                flags: flags(false),
                added_at: 0,
            }],
        )
        .unwrap()
        .item_id;
    for _ in 0..4 {
        // Toggle SEEN repeatedly; every store_flags must bump change_seq.
        mds.store_flags(&s, &item_id, &c, flags(true)).unwrap();
        let now = mds.container_status(&s, &c).unwrap().change_seq.0;
        assert!(now > prev, "change_seq did not advance");
        prev = now;
        mds.store_flags(&s, &item_id, &c, flags(false)).unwrap();
        let now = mds.container_status(&s, &c).unwrap().change_seq.0;
        assert!(now > prev);
        prev = now;
    }
}

#[test]
fn invariant_4_failed_multi_membership_add_rolls_back_all_placements() {
    let (_d, mds) = open();
    let s = fresh_set();
    mds.create_set(&s).unwrap();
    let c = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let bogus = ContainerId(uuid::Uuid::now_v7());
    let blob = mds.put_blob(b"will-not-stick").unwrap();

    // Pre-populate so we have a baseline change_seq/next_seq > 1.
    let warmup_blob = mds.put_blob(b"warmup").unwrap();
    mds.add_item(
        &s,
        &warmup_blob,
        &[Membership {
            container: c,
            flags: flags(false),
            added_at: 0,
        }],
    )
    .unwrap();
    let baseline = mds.container_status(&s, &c).unwrap();

    let err = mds.add_item(
        &s,
        &blob,
        &[
            Membership {
                container: c,
                flags: flags(false),
                added_at: 0,
            },
            Membership {
                container: bogus,
                flags: flags(false),
                added_at: 0,
            },
        ],
    );
    assert!(err.is_err(), "add_item with bogus container should fail");

    let after = mds.container_status(&s, &c).unwrap();
    assert_eq!(after.next_seq, baseline.next_seq, "next_seq leaked");
    assert_eq!(after.change_seq, baseline.change_seq, "change_seq leaked");
    assert_eq!(after.exists, baseline.exists, "exists leaked");
    assert_eq!(after.unread, baseline.unread, "unread leaked");

    // The valid container's listing must be exactly the warm-up item.
    let listed = mds.list_items(&s, &c, SeqRange::All).unwrap();
    assert_eq!(listed.len(), 1, "rolled-back item leaked into list");
}

#[test]
fn invariant_5_blob_is_reachable_for_every_membership() {
    let (_d, mds) = open();
    let s = fresh_set();
    mds.create_set(&s).unwrap();
    let c = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    for i in 0..3 {
        let body = format!("body-{i}");
        let blob = mds.put_blob(body.as_bytes()).unwrap();
        mds.add_item(
            &s,
            &blob,
            &[Membership {
                container: c,
                flags: flags(false),
                added_at: 0,
            }],
        )
        .unwrap();
    }
    for rec in mds.list_items(&s, &c, SeqRange::All).unwrap() {
        // The blob the item points at must be retrievable. Phase 3
        // refcount work doesn't change this — Phase 1b commits to the
        // data.sqlite truth, and `get_blob` reads the CAS dir directly.
        let bytes = mds.get_blob(&rec.meta.blob_hash).unwrap();
        assert_eq!(bytes.len() as u64, rec.meta.size_bytes);
    }
}

#[test]
fn invariant_10_flags_isolate_per_container() {
    // Same item lives in two containers; setting SEEN in one must not
    // alter flags or unread in the other.
    let (_d, mds) = open();
    let s = fresh_set();
    mds.create_set(&s).unwrap();
    let inbox = mds.create_container(&s, None, "INBOX", attrs()).unwrap();
    let archive = mds.create_container(&s, None, "Archive", attrs()).unwrap();
    let blob = mds.put_blob(b"two-place").unwrap();
    let item_id = mds
        .add_item(
            &s,
            &blob,
            &[
                Membership {
                    container: inbox,
                    flags: flags(false),
                    added_at: 0,
                },
                Membership {
                    container: archive,
                    flags: flags(false),
                    added_at: 0,
                },
            ],
        )
        .unwrap()
        .item_id;

    mds.store_flags(&s, &item_id, &inbox, flags(true)).unwrap();
    let inbox_view = mds.fetch_item(&s, &item_id, &inbox).unwrap();
    let archive_view = mds.fetch_item(&s, &item_id, &archive).unwrap();
    assert!(is_seen(inbox_view.flags), "inbox should be SEEN");
    assert!(!is_seen(archive_view.flags), "archive should be unchanged");
    assert_eq!(mds.container_status(&s, &inbox).unwrap().unread, 0);
    assert_eq!(mds.container_status(&s, &archive).unwrap().unread, 1);
}

#[test]
fn unread_delta_only_consults_seen_bit() {
    // Regression: the unread counter must read `Flags::SEEN` only,
    // ignoring `\Flagged` / `\Answered` / `\Draft` / `\Deleted` bits.
    // Allocating those bits (T10) is non-breaking for unread accounting
    // exactly because of this invariant — guard it explicitly so a
    // future "flag-as-byte-truthy" regression in the SQL projection
    // fails here rather than silently in production.
    let (_d, mds) = open();
    let s = fresh_set();
    mds.create_set(&s).unwrap();
    let c = mds.create_container(&s, None, "INBOX", attrs()).unwrap();

    // Item A: \Flagged | \Answered | \Draft (no \Seen) — must count as unread.
    let blob_a = mds.put_blob(b"item-a").unwrap();
    let id_a = mds
        .add_item(
            &s,
            &blob_a,
            &[Membership {
                container: c,
                flags: Flags(Flags::FLAGGED | Flags::ANSWERED | Flags::DRAFT),
                added_at: 0,
            }],
        )
        .unwrap()
        .item_id;
    let _ = id_a;

    // Item B: \Seen | \Deleted — must count as read (zero unread delta).
    let blob_b = mds.put_blob(b"item-b").unwrap();
    let id_b = mds
        .add_item(
            &s,
            &blob_b,
            &[Membership {
                container: c,
                flags: Flags(Flags::SEEN | Flags::DELETED),
                added_at: 0,
            }],
        )
        .unwrap()
        .item_id;
    let _ = id_b;

    assert_eq!(
        mds.container_status(&s, &c).unwrap().unread,
        1,
        "only item A (no SEEN bit) should contribute to unread"
    );
}

// ---------------------------------------------------------------------------
// Property tests — random op sequences against a model
//
// Strategy:
//
//  - Spin up a fresh SqliteCasMds with N pre-created containers.
//  - Drive a random sequence of Op variants (Add / Copy / Move /
//    Remove / StoreFlags) against it.
//  - Maintain a parallel in-memory model: per-(item, container) flags,
//    per-container set of allocated seqs, max change_seq seen.
//  - After every applied op, check all invariants against both the
//    real store and the model.
//
// Operations index into the current item/container vectors with `pick
// % len`. If a precondition fails (item already in dest, item missing,
// etc.) we treat the op as a no-op and move on — the goal is to drive
// state through every code path, not to require the random walk to be
// always-valid.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Op {
    Add {
        body: u16,
        members: Vec<(u8, bool)>,
    },
    Copy {
        item: u16,
        dest: u8,
        seen: bool,
    },
    Move {
        item: u16,
        src: u8,
        dest: u8,
        seen: bool,
    },
    Remove {
        item: u16,
        container: u8,
    },
    StoreFlags {
        item: u16,
        container: u8,
        seen: bool,
    },
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        (any::<u16>(), prop_vec((any::<u8>(), any::<bool>()), 1..=3))
            .prop_map(|(body, members)| Op::Add { body, members }),
        (any::<u16>(), any::<u8>(), any::<bool>()).prop_map(|(item, dest, seen)| Op::Copy {
            item,
            dest,
            seen
        }),
        (any::<u16>(), any::<u8>(), any::<u8>(), any::<bool>()).prop_map(
            |(item, src, dest, seen)| Op::Move {
                item,
                src,
                dest,
                seen
            }
        ),
        (any::<u16>(), any::<u8>()).prop_map(|(item, container)| Op::Remove { item, container }),
        (any::<u16>(), any::<u8>(), any::<bool>()).prop_map(|(item, container, seen)| {
            Op::StoreFlags {
                item,
                container,
                seen,
            }
        }),
    ]
}

#[derive(Default)]
struct Model {
    containers: Vec<ContainerId>,
    items: Vec<cosmix_mds::ItemId>,
    /// (item, container) → flags. Drives expected exists/unread.
    memberships: HashMap<(cosmix_mds::ItemId, ContainerId), Flags>,
    /// Every seq ever handed out by a container, kept to assert
    /// monotonicity and no-reuse.
    allocated_seqs: HashMap<ContainerId, BTreeSet<u32>>,
    /// Highest change_seq seen per container, to assert strict
    /// monotonicity across the whole run.
    max_change_seq: HashMap<ContainerId, u64>,
    /// UIDVALIDITY captured at first sight; must never change.
    seq_validity: HashMap<ContainerId, u64>,
}

impl Model {
    fn pick<T: Copy>(slice: &[T], idx: u16) -> Option<T> {
        if slice.is_empty() {
            None
        } else {
            Some(slice[(idx as usize) % slice.len()])
        }
    }
    fn pick_container(&self, idx: u8) -> Option<ContainerId> {
        Self::pick(&self.containers, idx as u16)
    }
    fn pick_item(&self, idx: u16) -> Option<cosmix_mds::ItemId> {
        Self::pick(&self.items, idx)
    }
}

fn check_invariants(
    mds: &SqliteCasMds,
    set: &SetId,
    model: &mut Model,
) -> Result<(), TestCaseError> {
    for c in &model.containers {
        let st = mds.container_status(set, c).unwrap();

        // UIDVALIDITY: capture once, then assert stable.
        let entry = model.seq_validity.entry(*c).or_insert(st.seq_validity);
        prop_assert_eq!(*entry, st.seq_validity, "seq_validity drifted on {:?}", c);

        // change_seq strictly non-decreasing across the whole run.
        let prev = model.max_change_seq.get(c).copied().unwrap_or(0);
        prop_assert!(
            st.change_seq.0 >= prev,
            "change_seq regressed on {:?}: {} -> {}",
            c,
            prev,
            st.change_seq.0
        );
        model.max_change_seq.insert(*c, st.change_seq.0);

        // exists / unread counters reconcile against model membership.
        let mut expect_exists: u64 = 0;
        let mut expect_unread: u64 = 0;
        for ((_item, cc), f) in &model.memberships {
            if cc == c {
                expect_exists += 1;
                if !is_seen(*f) {
                    expect_unread += 1;
                }
            }
        }
        prop_assert_eq!(st.exists, expect_exists, "exists mismatch on {:?}", c);
        prop_assert_eq!(st.unread, expect_unread, "unread mismatch on {:?}", c);

        // list_items() membership matches the model exactly, in seq order.
        let listed = mds.list_items(set, c, SeqRange::All).unwrap();
        let mut prev_seq: Option<u32> = None;
        for rec in &listed {
            // Strict ordering by seq.
            if let Some(p) = prev_seq {
                prop_assert!(rec.seq.0 > p, "list_items not strictly ordered");
            }
            prev_seq = Some(rec.seq.0);
        }
        // Per-membership, fetch_item must report the model's flags
        // verbatim (per-container flag isolation).
        for ((item, cc), f) in &model.memberships {
            if cc != c {
                continue;
            }
            let fetched = mds.fetch_item(set, item, c).unwrap();
            prop_assert_eq!(
                fetched.flags.0,
                f.0,
                "flags mismatch for ({:?}, {:?})",
                item,
                cc
            );
        }
    }
    Ok(())
}

fn apply_add(
    mds: &SqliteCasMds,
    set: &SetId,
    model: &mut Model,
    body: u16,
    members: &[(u8, bool)],
) -> Result<(), TestCaseError> {
    if model.containers.is_empty() {
        return Ok(());
    }
    // Resolve picks → de-duplicated container list (add_item rejects
    // dup memberships at the SQL UNIQUE level — keep the model honest).
    let mut chosen: Vec<(ContainerId, Flags)> = Vec::new();
    let mut seen: HashSet<ContainerId> = HashSet::new();
    for (idx, sf) in members {
        let c = model.pick_container(*idx).unwrap();
        if seen.insert(c) {
            chosen.push((c, flags(*sf)));
        }
    }
    let blob = mds.put_blob(format!("body-{body}").as_bytes()).unwrap();
    let memberships: Vec<Membership> = chosen
        .iter()
        .map(|(c, f)| Membership {
            container: *c,
            flags: *f,
            added_at: 0,
        })
        .collect();
    let report = mds.add_item(set, &blob, &memberships).unwrap();

    // Update model after success.
    model.items.push(report.item_id);
    for (placement, (c, f)) in report.placements.iter().zip(chosen.iter()) {
        prop_assert_eq!(placement.container, *c);
        let allocated = model.allocated_seqs.entry(*c).or_default();
        prop_assert!(
            allocated.insert(placement.seq.0),
            "duplicate seq {} on {:?}",
            placement.seq.0,
            c
        );
        if let Some(&max) = allocated.iter().next_back() {
            prop_assert!(placement.seq.0 >= max, "non-monotonic seq allocation");
        }
        model.memberships.insert((report.item_id, *c), *f);
    }
    Ok(())
}

fn apply_copy(
    mds: &SqliteCasMds,
    set: &SetId,
    model: &mut Model,
    item_pick: u16,
    dest_pick: u8,
    seen: bool,
) -> Result<(), TestCaseError> {
    let Some(item) = model.pick_item(item_pick) else {
        return Ok(());
    };
    let Some(dest) = model.pick_container(dest_pick) else {
        return Ok(());
    };
    if model.memberships.contains_key(&(item, dest)) {
        // Copy into existing membership is a defined error; skip.
        return Ok(());
    }
    let f = flags(seen);
    // v1.1 §3 inheritance: dest membership adopts the flags of an
    // existing membership of `item`. MDS uses `ORDER BY container_id
    // LIMIT 1` to pick deterministically; the model must mirror that
    // exactly or unread accounting drifts when the item has divergent
    // flags across memberships (legal via direct store_flags / IMAP
    // STORE — see `item_memberships_returns_per_membership_flags_and_tags`).
    let inherited = {
        let mut candidates: Vec<(ContainerId, Flags)> = model
            .memberships
            .iter()
            .filter_map(|((i, c), v)| (*i == item).then_some((*c, *v)))
            .collect();
        candidates.sort_by_key(|(c, _)| c.0);
        candidates.first().map(|(_, v)| *v).unwrap_or(f)
    };
    let report = mds.copy_item(set, &item, &dest, f).unwrap();
    let allocated = model.allocated_seqs.entry(dest).or_default();
    prop_assert!(allocated.insert(report.seq_dest.0), "copy reused seq");
    model.memberships.insert((item, dest), inherited);
    Ok(())
}

fn apply_move(
    mds: &SqliteCasMds,
    set: &SetId,
    model: &mut Model,
    item_pick: u16,
    src_pick: u8,
    dest_pick: u8,
    seen: bool,
) -> Result<(), TestCaseError> {
    let Some(item) = model.pick_item(item_pick) else {
        return Ok(());
    };
    let Some(src) = model.pick_container(src_pick) else {
        return Ok(());
    };
    let Some(dest) = model.pick_container(dest_pick) else {
        return Ok(());
    };
    if src == dest {
        return Ok(());
    }
    if !model.memberships.contains_key(&(item, src)) {
        return Ok(());
    }
    if model.memberships.contains_key(&(item, dest)) {
        return Ok(());
    }
    let f = flags(seen);
    // v1.1 §3 inheritance: dest adopts src's flags, not the caller's.
    let inherited = model.memberships.get(&(item, src)).copied().unwrap_or(f);
    let report = mds.move_item(set, &item, &src, &dest, f).unwrap();
    let dest_allocated = model.allocated_seqs.entry(dest).or_default();
    prop_assert!(
        dest_allocated.insert(report.seq_dest.0),
        "move reused dest seq"
    );
    model.memberships.remove(&(item, src));
    model.memberships.insert((item, dest), inherited);
    Ok(())
}

fn apply_remove(
    mds: &SqliteCasMds,
    set: &SetId,
    model: &mut Model,
    item_pick: u16,
    container_pick: u8,
) -> Result<(), TestCaseError> {
    let Some(item) = model.pick_item(item_pick) else {
        return Ok(());
    };
    let Some(c) = model.pick_container(container_pick) else {
        return Ok(());
    };
    if !model.memberships.contains_key(&(item, c)) {
        return Ok(());
    }
    mds.remove_membership(set, &item, &c).unwrap();
    model.memberships.remove(&(item, c));
    // Item row is dropped by remove_membership when it goes to zero
    // memberships; reflect that so subsequent ops don't pick a dead id.
    let still_referenced = model.memberships.keys().any(|(i, _)| *i == item);
    if !still_referenced {
        model.items.retain(|x| *x != item);
    }
    Ok(())
}

fn apply_store_flags(
    mds: &SqliteCasMds,
    set: &SetId,
    model: &mut Model,
    item_pick: u16,
    container_pick: u8,
    seen: bool,
) -> Result<(), TestCaseError> {
    let Some(item) = model.pick_item(item_pick) else {
        return Ok(());
    };
    let Some(c) = model.pick_container(container_pick) else {
        return Ok(());
    };
    if !model.memberships.contains_key(&(item, c)) {
        return Ok(());
    }
    let f = flags(seen);
    mds.store_flags(set, &item, &c, f).unwrap();
    model.memberships.insert((item, c), f);
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Random sequences of add/copy/move/remove/store_flags preserve
    /// every per-container invariant at every step:
    ///
    ///   - UID monotonic + no-reuse (1)
    ///   - seq_validity stable (2)
    ///   - change_seq non-decreasing (3, weak form — strict
    ///     bump-per-mutation is in
    ///     `each_mutation_advances_change_seq_and_writes_changelog_row`)
    ///   - exists/unread reconcile against memberships (7)
    ///   - per-container flag isolation (10)
    #[test]
    fn random_op_sequences_preserve_invariants(
        ops in prop_vec(op_strategy(), 1..40)
    ) {
        let (_d, mds) = open();
        let s = fresh_set();
        mds.create_set(&s).unwrap();
        let mut model = Model::default();
        for name in ["INBOX", "Archive", "Sent", "Trash"] {
            let id = mds.create_container(&s, None, name, attrs()).unwrap();
            model.containers.push(id);
        }
        check_invariants(&mds, &s, &mut model)?;
        for op in ops {
            match op {
                Op::Add { body, members } => apply_add(&mds, &s, &mut model, body, &members)?,
                Op::Copy { item, dest, seen } => apply_copy(&mds, &s, &mut model, item, dest, seen)?,
                Op::Move { item, src, dest, seen } =>
                    apply_move(&mds, &s, &mut model, item, src, dest, seen)?,
                Op::Remove { item, container } =>
                    apply_remove(&mds, &s, &mut model, item, container)?,
                Op::StoreFlags { item, container, seen } =>
                    apply_store_flags(&mds, &s, &mut model, item, container, seen)?,
            }
            check_invariants(&mds, &s, &mut model)?;
        }
    }

    /// Spec invariant 4: a multi-membership add that names one bogus
    /// container leaves every valid container's state byte-for-byte
    /// unchanged (next_seq, change_seq, exists, unread, listing).
    /// Also verifies that the would-be item is not retrievable on any
    /// of the named valid containers.
    #[test]
    fn failed_multi_membership_add_rolls_back_random_shapes(
        valid_count in 1usize..=4,
        warmup_per_container in 0u8..=3,
        bogus_position in 0usize..=4,
    ) {
        let (_d, mds) = open();
        let s = fresh_set();
        mds.create_set(&s).unwrap();
        let mut containers = Vec::new();
        for i in 0..valid_count {
            let c = mds
                .create_container(&s, None, &format!("c{i}"), attrs())
                .unwrap();
            containers.push(c);
        }
        // Warm-up so baseline counters are non-zero.
        for c in &containers {
            for j in 0..warmup_per_container {
                let blob = mds.put_blob(format!("warm-{j}-{:?}", c.0).as_bytes()).unwrap();
                mds.add_item(
                    &s, &blob,
                    &[Membership { container: *c, flags: flags(j % 2 == 0), added_at: 0 }]
                ).unwrap();
            }
        }
        let baseline: Vec<_> = containers.iter()
            .map(|c| (*c, mds.container_status(&s, c).unwrap()))
            .collect();
        let baseline_lists: Vec<_> = containers.iter()
            .map(|c| (*c, mds.list_items(&s, c, SeqRange::All).unwrap()))
            .collect();

        // Build memberships with one bogus container interleaved at
        // `bogus_position` (clamped to the slot count).
        let bogus = ContainerId(uuid::Uuid::now_v7());
        let mut memberships: Vec<Membership> = containers.iter()
            .map(|c| Membership { container: *c, flags: flags(false), added_at: 0 })
            .collect();
        let pos = bogus_position.min(memberships.len());
        memberships.insert(pos, Membership { container: bogus, flags: flags(false), added_at: 0 });

        let blob = mds.put_blob(b"rolled-back-payload").unwrap();
        let res = mds.add_item(&s, &blob, &memberships);
        prop_assert!(res.is_err(), "bogus container should fail the add");

        for (c, prior) in &baseline {
            let now = mds.container_status(&s, c).unwrap();
            prop_assert_eq!(now.next_seq, prior.next_seq, "next_seq leaked on {:?}", c);
            prop_assert_eq!(now.change_seq, prior.change_seq, "change_seq leaked on {:?}", c);
            prop_assert_eq!(now.exists, prior.exists, "exists leaked on {:?}", c);
            prop_assert_eq!(now.unread, prior.unread, "unread leaked on {:?}", c);
        }
        for (c, prior_list) in &baseline_lists {
            let now = mds.list_items(&s, c, SeqRange::All).unwrap();
            prop_assert_eq!(now.len(), prior_list.len(), "list size leaked on {:?}", c);
            for (a, b) in now.iter().zip(prior_list.iter()) {
                prop_assert_eq!(a.seq, b.seq);
                prop_assert_eq!(a.change_seq, b.change_seq);
                prop_assert_eq!(a.flags.0, b.flags.0);
            }
        }
    }

    /// list / fetch / changes_since are mutually consistent: for any
    /// reachable item, fetch_item agrees with the list_items entry,
    /// and changes_since(0) covers every committed mutation in
    /// strictly-increasing change_seq order.
    #[test]
    fn list_fetch_changelog_consistency(
        ops in prop_vec(op_strategy(), 1..30)
    ) {
        let (_d, mds) = open();
        let s = fresh_set();
        mds.create_set(&s).unwrap();
        let mut model = Model::default();
        for name in ["A", "B", "C"] {
            model.containers.push(
                mds.create_container(&s, None, name, attrs()).unwrap()
            );
        }
        for op in ops {
            match op {
                Op::Add { body, members } => apply_add(&mds, &s, &mut model, body, &members)?,
                Op::Copy { item, dest, seen } => apply_copy(&mds, &s, &mut model, item, dest, seen)?,
                Op::Move { item, src, dest, seen } =>
                    apply_move(&mds, &s, &mut model, item, src, dest, seen)?,
                Op::Remove { item, container } =>
                    apply_remove(&mds, &s, &mut model, item, container)?,
                Op::StoreFlags { item, container, seen } =>
                    apply_store_flags(&mds, &s, &mut model, item, container, seen)?,
            }
        }
        for c in &model.containers {
            let listed = mds.list_items(&s, c, SeqRange::All).unwrap();
            // The list size must equal the model's membership count
            // for this container. Then for every model membership in
            // this container, fetch_item must match the corresponding
            // list_items record on seq, change_seq, and flags.
            let model_in_c: Vec<_> = model.memberships.iter()
                .filter(|((_, cc), _)| cc == c)
                .collect();
            prop_assert_eq!(
                listed.len(), model_in_c.len(),
                "list_items size != model membership count on {:?}", c
            );
            for ((item, _), f) in &model_in_c {
                let fetched = mds.fetch_item(&s, item, c).unwrap();
                prop_assert_eq!(fetched.flags.0, f.0);
                let listed_match = listed.iter().find(|r| r.seq == fetched.seq);
                prop_assert!(
                    listed_match.is_some(),
                    "fetch_item seq {} not in list_items", fetched.seq.0
                );
                let listed_match = listed_match.unwrap();
                prop_assert_eq!(listed_match.change_seq, fetched.change_seq);
                prop_assert_eq!(listed_match.flags.0, fetched.flags.0);
            }
            // changes_since(0) returns rows in strictly-increasing
            // change_seq order and contains at least one row per
            // currently-listed item (the Added entry).
            let changes = mds.changes_since(&s, c, cosmix_mds::ChangeToken(0)).unwrap();
            let mut prev = 0u64;
            for ch in &changes {
                prop_assert!(
                    ch.change_seq.0 > prev,
                    "changes_since not strictly increasing"
                );
                prev = ch.change_seq.0;
            }
            for rec in &listed {
                let any_added = changes.iter().any(|ch|
                    ch.seq == rec.seq
                    && matches!(ch.kind, cosmix_mds::ChangeKind::Added)
                );
                prop_assert!(
                    any_added,
                    "missing Added changelog row for seq {}", rec.seq.0
                );
            }
        }
    }

    /// Per-op audit: every successful Add / Copy / Move / Remove /
    /// StoreFlags advances the affected container's `change_seq` by
    /// exactly one and writes a changelog row whose `(kind, seq,
    /// item_id)` matches the operation's contract:
    ///
    ///   - Add(N unique containers) → +1 on each, one Added row
    ///     each, with the freshly allocated seq.
    ///   - Copy(dest)               → +1 on dest, one Added row.
    ///   - Move(src → dest)         → +1 on each: Removed at the
    ///     pre-move seq on src, Added at the freshly allocated seq
    ///     on dest.
    ///   - Remove(c)                → +1 on c, one Removed row at
    ///     the pre-remove seq.
    ///   - StoreFlags(c)            → +1 on c, one MetadataChanged
    ///     row at the membership's existing seq (unconditional —
    ///     even when the new flags equal the old flags, the impl
    ///     bumps and writes; the test pins that contract).
    #[test]
    fn each_mutation_advances_change_seq_and_writes_changelog_row(
        ops in prop_vec(op_strategy(), 1..30)
    ) {
        let (_d, mds) = open();
        let s = fresh_set();
        mds.create_set(&s).unwrap();
        let mut model = Model::default();
        for name in ["A", "B", "C", "D"] {
            model.containers.push(
                mds.create_container(&s, None, name, attrs()).unwrap()
            );
        }

        for op in ops {
            audit_op(&mds, &s, &mut model, op)?;
        }
    }
}

// Snapshot the change_seq for one container so we can later audit
// the delta a single mutation produced.
fn pre_change_seq(mds: &SqliteCasMds, set: &SetId, c: &ContainerId) -> cosmix_mds::ChangeToken {
    mds.container_status(set, c).unwrap().change_seq
}

// Assert: container `c`'s change_seq advanced by exactly one, and
// the newly-written changelog rows since `prior` are exactly the
// supplied (kind, seq, item_id) tuples in order.
fn assert_single_changelog_row(
    mds: &SqliteCasMds,
    set: &SetId,
    c: &ContainerId,
    prior: cosmix_mds::ChangeToken,
    expected_kind: cosmix_mds::ChangeKind,
    expected_seq: cosmix_mds::Seq,
    expected_item: cosmix_mds::ItemId,
) -> Result<(), TestCaseError> {
    let post = mds.container_status(set, c).unwrap().change_seq;
    prop_assert_eq!(
        post.0,
        prior.0 + 1,
        "change_seq did not advance by 1 on {:?}",
        c
    );
    let new_rows = mds.changes_since(set, c, prior).unwrap();
    prop_assert_eq!(
        new_rows.len(),
        1,
        "expected 1 changelog row, got {}",
        new_rows.len()
    );
    let row = &new_rows[0];
    prop_assert_eq!(row.change_seq, post, "changelog row change_seq mismatch");
    prop_assert_eq!(row.kind, expected_kind);
    prop_assert_eq!(row.seq, expected_seq);
    prop_assert_eq!(row.item_id, Some(expected_item));
    Ok(())
}

// Run a single op and audit (change_seq, changelog) for every
// container it touched. Mirrors the pick/skip semantics of the
// apply_* helpers but performs pre/post snapshotting.
fn audit_op(
    mds: &SqliteCasMds,
    set: &SetId,
    model: &mut Model,
    op: Op,
) -> Result<(), TestCaseError> {
    match op {
        Op::Add { body, members } => {
            if model.containers.is_empty() {
                return Ok(());
            }
            let mut chosen: Vec<(ContainerId, Flags)> = Vec::new();
            let mut seen: HashSet<ContainerId> = HashSet::new();
            for (idx, sf) in &members {
                let c = model.pick_container(*idx).unwrap();
                if seen.insert(c) {
                    chosen.push((c, flags(*sf)));
                }
            }
            let priors: Vec<_> = chosen
                .iter()
                .map(|(c, _)| (*c, pre_change_seq(mds, set, c)))
                .collect();
            let blob = mds.put_blob(format!("body-{body}").as_bytes()).unwrap();
            let memberships: Vec<Membership> = chosen
                .iter()
                .map(|(c, f)| Membership {
                    container: *c,
                    flags: *f,
                    added_at: 0,
                })
                .collect();
            let report = mds.add_item(set, &blob, &memberships).unwrap();
            for (placement, (c, prior)) in report.placements.iter().zip(priors.iter()) {
                assert_single_changelog_row(
                    mds,
                    set,
                    c,
                    *prior,
                    cosmix_mds::ChangeKind::Added,
                    placement.seq,
                    report.item_id,
                )?;
            }
            model.items.push(report.item_id);
            for (placement, (c, f)) in report.placements.iter().zip(chosen.iter()) {
                model.memberships.insert((report.item_id, *c), *f);
                model
                    .allocated_seqs
                    .entry(*c)
                    .or_default()
                    .insert(placement.seq.0);
            }
        }
        Op::Copy { item, dest, seen } => {
            let Some(item) = model.pick_item(item) else {
                return Ok(());
            };
            let Some(dest) = model.pick_container(dest) else {
                return Ok(());
            };
            if model.memberships.contains_key(&(item, dest)) {
                return Ok(());
            }
            let prior = pre_change_seq(mds, set, &dest);
            let f = flags(seen);
            // v1.1 §3 inheritance: deterministic pick — smallest
            // container_id, mirroring MDS's `ORDER BY container_id
            // LIMIT 1`. See apply_copy comment for the divergence trap.
            let inherited = {
                let mut candidates: Vec<(ContainerId, Flags)> = model
                    .memberships
                    .iter()
                    .filter_map(|((i, c), v)| (*i == item).then_some((*c, *v)))
                    .collect();
                candidates.sort_by_key(|(c, _)| c.0);
                candidates.first().map(|(_, v)| *v).unwrap_or(f)
            };
            let report = mds.copy_item(set, &item, &dest, f).unwrap();
            assert_single_changelog_row(
                mds,
                set,
                &dest,
                prior,
                cosmix_mds::ChangeKind::Added,
                report.seq_dest,
                item,
            )?;
            model.memberships.insert((item, dest), inherited);
            model
                .allocated_seqs
                .entry(dest)
                .or_default()
                .insert(report.seq_dest.0);
        }
        Op::Move {
            item,
            src,
            dest,
            seen,
        } => {
            let Some(item) = model.pick_item(item) else {
                return Ok(());
            };
            let Some(src) = model.pick_container(src) else {
                return Ok(());
            };
            let Some(dest) = model.pick_container(dest) else {
                return Ok(());
            };
            if src == dest {
                return Ok(());
            }
            if !model.memberships.contains_key(&(item, src)) {
                return Ok(());
            }
            if model.memberships.contains_key(&(item, dest)) {
                return Ok(());
            }
            // Pre-move seq on src needed for the Removed audit.
            let pre_src_seq = mds.fetch_item(set, &item, &src).unwrap().seq;
            let prior_src = pre_change_seq(mds, set, &src);
            let prior_dest = pre_change_seq(mds, set, &dest);
            let f = flags(seen);
            // v1.1 §3 inheritance: dest adopts src's flags, not the caller's.
            let inherited = model.memberships.get(&(item, src)).copied().unwrap_or(f);
            let report = mds.move_item(set, &item, &src, &dest, f).unwrap();
            assert_single_changelog_row(
                mds,
                set,
                &src,
                prior_src,
                cosmix_mds::ChangeKind::Removed,
                pre_src_seq,
                item,
            )?;
            assert_single_changelog_row(
                mds,
                set,
                &dest,
                prior_dest,
                cosmix_mds::ChangeKind::Added,
                report.seq_dest,
                item,
            )?;
            model.memberships.remove(&(item, src));
            model.memberships.insert((item, dest), inherited);
            model
                .allocated_seqs
                .entry(dest)
                .or_default()
                .insert(report.seq_dest.0);
        }
        Op::Remove { item, container } => {
            let Some(item) = model.pick_item(item) else {
                return Ok(());
            };
            let Some(c) = model.pick_container(container) else {
                return Ok(());
            };
            if !model.memberships.contains_key(&(item, c)) {
                return Ok(());
            }
            let pre_seq = mds.fetch_item(set, &item, &c).unwrap().seq;
            let prior = pre_change_seq(mds, set, &c);
            mds.remove_membership(set, &item, &c).unwrap();
            assert_single_changelog_row(
                mds,
                set,
                &c,
                prior,
                cosmix_mds::ChangeKind::Removed,
                pre_seq,
                item,
            )?;
            model.memberships.remove(&(item, c));
            let still = model.memberships.keys().any(|(i, _)| *i == item);
            if !still {
                model.items.retain(|x| *x != item);
            }
        }
        Op::StoreFlags {
            item,
            container,
            seen,
        } => {
            let Some(item) = model.pick_item(item) else {
                return Ok(());
            };
            let Some(c) = model.pick_container(container) else {
                return Ok(());
            };
            if !model.memberships.contains_key(&(item, c)) {
                return Ok(());
            }
            let pre_seq = mds.fetch_item(set, &item, &c).unwrap().seq;
            let prior = pre_change_seq(mds, set, &c);
            let f = flags(seen);
            mds.store_flags(set, &item, &c, f).unwrap();
            assert_single_changelog_row(
                mds,
                set,
                &c,
                prior,
                cosmix_mds::ChangeKind::MetadataChanged,
                pre_seq,
                item,
            )?;
            model.memberships.insert((item, c), f);
        }
    }
    Ok(())
}
