//! `ot::txn_sequence` round trips (ced E1 plan Stage S, codex round-2 N2):
//! the client-side sequence of a base-coordinate transaction must produce
//! exactly the text the server produces for the same request, keep the
//! request-order tie rule, and pair every step with its own item.

use cosmix_edit_core::buffer::{Buffer, Cas, OpSpec, TxnRequest};
use cosmix_edit_core::origin::{Origin, OriginKind, Via};
use cosmix_edit_core::ot::{Overlap, txn_sequence};
use cosmix_edit_core::pos::{PosSpec, RangeSpec};
use cosmix_edit_core::text::Text;

fn apply_seq(base: &str, items: &[(std::ops::Range<usize>, String)]) -> (String, Vec<usize>) {
    let seq = txn_sequence(items).expect("no overlap");
    let order: Vec<usize> = seq.iter().map(|(i, _)| *i).collect();
    let mut text = Text::from_text(base).unwrap();
    let prepared = text.prepare(seq.into_iter().map(|(_, e)| e).collect()).unwrap();
    text.commit(prepared);
    let mut out = String::new();
    text.read(0..text.len(), &mut out);
    (out, order)
}

fn server(base: &str, items: &[(std::ops::Range<usize>, String)]) -> String {
    let (mut buf, _) = Buffer::from_bytes(base.as_bytes()).unwrap();
    let ops = items
        .iter()
        .map(|(r, t)| {
            if r.is_empty() {
                OpSpec::Insert { at: PosSpec::Offset(r.start), text: t.clone() }
            } else if t.is_empty() {
                OpSpec::Delete { range: RangeSpec::Offsets([r.start, r.end]) }
            } else {
                OpSpec::Replace { range: RangeSpec::Offsets([r.start, r.end]), text: t.clone() }
            }
        })
        .collect();
    let req = TxnRequest { ops, cas: Cas::Latest, coalesce: false, cursor: None, op_id: None };
    let origin = Origin::new(OriginKind::Agent, "test");
    buf.apply(req, &origin, Via::default(), 0).unwrap();
    let mut out = String::new();
    let len = buf.len();
    buf.read(0..len, &mut out);
    out
}

fn items(v: &[(usize, usize, &str)]) -> Vec<(std::ops::Range<usize>, String)> {
    v.iter().map(|&(s, e, t)| (s..e, t.to_string())).collect()
}

const BASE: &str = "0123456789abcdefghijXYZ";

#[test]
fn equal_offset_inserts_read_in_request_order() {
    let it = items(&[(10, 10, "A"), (10, 10, "B")]);
    let (out, order) = apply_seq(BASE, &it);
    assert_eq!(out, "0123456789ABabcdefghijXYZ");
    assert_eq!(order, vec![1, 0], "reverse request order at one offset");
    assert_eq!(out, server(BASE, &it));
}

#[test]
fn insert_at_range_start_reads_before_the_replacement() {
    let it = items(&[(10, 20, ""), (10, 10, "bar")]);
    let (out, _) = apply_seq(BASE, &it);
    assert_eq!(out, "0123456789barXYZ");
    assert_eq!(out, server(BASE, &it));

    let it = items(&[(10, 20, "R"), (10, 10, "A")]);
    let (out, order) = apply_seq(BASE, &it);
    assert_eq!(out, "0123456789ARXYZ");
    assert_eq!(order, vec![0, 1], "the range op applies first at an equal start");
    assert_eq!(out, server(BASE, &it));
}

#[test]
fn insert_at_range_end_reads_after_it() {
    let it = items(&[(10, 20, "R"), (20, 20, "E")]);
    let (out, _) = apply_seq(BASE, &it);
    assert_eq!(out, "0123456789REXYZ");
    assert_eq!(out, server(BASE, &it));
}

#[test]
fn steps_pair_with_their_items_for_deleted_text() {
    // Mixed transaction: every step's (offset, delete) must be the item's base
    // range, so the deleted text read from the BASE by item index is exactly
    // what the step removes.
    let it = items(&[(2, 4, "x"), (15, 15, "ins"), (8, 12, ""), (0, 0, "S")]);
    let seq = txn_sequence(&it).unwrap();
    for (i, e) in &seq {
        let (r, t) = &it[*i];
        assert_eq!((e.offset, e.delete, e.insert.as_str()), (r.start, r.end - r.start, t.as_str()));
        assert_eq!(&BASE[e.offset..e.offset + e.delete], &BASE[r.clone()]);
    }
    let offsets: Vec<usize> = seq.iter().map(|(_, e)| e.offset).collect();
    assert!(offsets.windows(2).all(|w| w[0] >= w[1]), "canonical: start descending");
    let (out, _) = apply_seq(BASE, &it);
    assert_eq!(out, server(BASE, &it));
}

#[test]
fn overlaps_are_refused_like_the_server() {
    assert_eq!(txn_sequence(&items(&[(2, 6, ""), (4, 8, "x")])), Err(Overlap));
    assert_eq!(txn_sequence(&items(&[(2, 6, "a"), (2, 3, "b")])), Err(Overlap), "equal non-empty starts");
    assert_eq!(txn_sequence(&items(&[(2, 6, ""), (4, 4, "i")])), Err(Overlap), "insert strictly inside");
    assert!(txn_sequence(&items(&[(2, 6, ""), (6, 6, "i"), (2, 2, "j")])).is_ok(), "inserts at both edges");
}

// ── transform_through_set (Stage S freeze note 3) ───────────────────────────

use cosmix_edit_core::ot::{Priority, transform_range, transform_through_set};

/// Server result: remote `x` applied first, then the transaction's items
/// rebased through it (ThroughFirst), then applied in canonical order.
fn server_order(base: &str, x: (usize, usize, &str), it: &[(std::ops::Range<usize>, String)]) -> String {
    let (xs, xe, xt) = x;
    let mut after_x = base.to_string();
    after_x.replace_range(xs..xe, xt);
    let xe_edit = cosmix_edit_core::ot::Edit { offset: xs, delete: xe - xs, insert: xt.to_string() };
    let rebased: Vec<_> = it
        .iter()
        .map(|(r, t)| (transform_range(r.clone(), &xe_edit, Priority::ThroughFirst).unwrap(), t.clone()))
        .collect();
    apply_seq(&after_x, &rebased).0
}

/// Client result: the transaction applied first (optimistic), then `x`
/// transformed through the item SET (SelfFirst) and applied.
fn client_order(base: &str, x: (usize, usize, &str), it: &[(std::ops::Range<usize>, String)]) -> String {
    let (view, _) = apply_seq(base, it);
    let set: Vec<(std::ops::Range<usize>, usize)> = it.iter().map(|(r, t)| (r.clone(), t.len())).collect();
    let (xs, xe, xt) = x;
    let r = transform_through_set(xs..xe, &set, Priority::SelfFirst).unwrap();
    let mut out = view;
    out.replace_range(r, xt);
    out
}

#[test]
fn remote_insert_after_a_deleted_range_lands_after_the_insert_at_its_start() {
    let base = "0123456789abcdefghijXYZ\n";
    let it = items(&[(10, 20, ""), (10, 10, "P")]);
    let server = server_order(base, (20, 20, "Z"), &it);
    assert_eq!(server, "0123456789PZXYZ\n");
    assert_eq!(client_order(base, (20, 20, "Z"), &it), server);
}

#[test]
fn set_transform_agrees_with_the_server_on_equal_offset_cases() {
    let base = "0123456789abcdefghijXYZ\n";
    for (x, it) in [
        ((10, 10, "X"), items(&[(10, 10, "A"), (10, 10, "B")])),
        ((0, 5, ""), items(&[(10, 20, "R"), (10, 10, "A")])),
        ((10, 10, "X"), items(&[(5, 10, ""), (10, 15, "k")])),
        ((21, 22, "y"), items(&[(10, 20, ""), (10, 10, "P"), (20, 20, "Q")])),
    ] {
        assert_eq!(client_order(base, x, &it), server_order(base, x, &it), "x={x:?} items={it:?}");
    }
}

#[test]
fn set_transform_reports_overlap() {
    assert_eq!(transform_through_set(12..14, &[(10..20, 0)], Priority::SelfFirst), Err(Overlap));
    assert_eq!(transform_through_set(15..15, &[(10..20, 3)], Priority::SelfFirst), Err(Overlap));
}
