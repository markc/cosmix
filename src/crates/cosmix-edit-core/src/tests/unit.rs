//! Unit tests (plan §6.1).

use std::path::Path;

use super::*;
use crate::anchor::{AnchorSpec, Bias, Selection};
use crate::buffer::{Eol, LaneSel};
use crate::error::reason;
use crate::history::EntryKind;
use crate::lang::detect;
use crate::limits::{MAX_BUFFER_BYTES, MAX_LINES, MAX_OPS_PER_TXN, MAX_REQUEST_TEXT_BYTES};
use crate::ot::{Edit, Priority, RangeSet, invert, transform_range, transform_set};
use crate::pos::{NamedPos, Point};
use crate::search::FindQuery;
use crate::text::Text;
use crate::vendor::msedit::gap_buffer::inject_commit_failure;

// ---- positions -------------------------------------------------------------

#[test]
fn crlf_columns_address_every_boundary() {
    let b = buf("a\r\nb");
    let pts: Vec<Point> = (0..=4).map(|o| b.point(o)).collect();
    assert_eq!(
        pts,
        vec![
            Point { offset: 0, line: 1, col: 1 },
            Point { offset: 1, line: 1, col: 2 },
            Point { offset: 2, line: 1, col: 3 },
            Point { offset: 3, line: 2, col: 1 },
            Point { offset: 4, line: 2, col: 2 },
        ]
    );
    for p in pts {
        assert_eq!(b.resolve_pos(&PosSpec::LineCol { line: p.line, col: Some(p.col) }).unwrap(), p.offset);
    }
    assert_code(b.resolve_pos(&PosSpec::LineCol { line: 1, col: Some(4) }), ErrorCode::InvalidArgument, reason::COL_OUT_OF_RANGE);
    assert_eq!(b.resolve_pos(&PosSpec::LineCol { line: 2, col: None }).unwrap(), 3);
}

#[test]
fn line_and_col_out_of_range_reasons() {
    let b = buf("ab\ncd");
    for line in [0, 3, 99] {
        assert_code(b.resolve_pos(&PosSpec::LineCol { line, col: None }), ErrorCode::InvalidArgument, reason::LINE_OUT_OF_RANGE);
    }
    assert_code(b.resolve_pos(&PosSpec::LineCol { line: 1, col: Some(0) }), ErrorCode::InvalidArgument, reason::COL_OUT_OF_RANGE);
    assert_code(b.resolve_pos(&PosSpec::LineCol { line: 2, col: Some(4) }), ErrorCode::InvalidArgument, reason::COL_OUT_OF_RANGE);
    assert_eq!(b.resolve_pos(&PosSpec::LineCol { line: 2, col: Some(3) }).unwrap(), 5);
    assert_code(b.resolve_range(&RangeSpec::Lines { lines: [2, 3] }), ErrorCode::InvalidArgument, reason::LINE_OUT_OF_RANGE);
    assert_code(b.resolve_range(&RangeSpec::Lines { lines: [2, 1] }), ErrorCode::InvalidArgument, reason::LINE_OUT_OF_RANGE);
    assert_eq!(b.resolve_range(&RangeSpec::Lines { lines: [1, 1] }).unwrap(), 0..3);
    assert_eq!(b.resolve_range(&RangeSpec::Lines { lines: [1, 2] }).unwrap(), 0..5);
    assert_eq!(b.resolve_pos(&PosSpec::Named(NamedPos::End)).unwrap(), 5);
    assert_code(b.resolve_pos(&PosSpec::Offset(6)), ErrorCode::InvalidArgument, reason::OFFSET_OUT_OF_RANGE);
    assert_code(b.resolve_pos(&PosSpec::Anchor { anchor: "nope".into() }), ErrorCode::NotFound, reason::UNKNOWN_ANCHOR);
}

#[test]
fn not_char_boundary() {
    let mut b = buf("é");
    assert_code(b.resolve_pos(&PosSpec::Offset(1)), ErrorCode::InvalidArgument, reason::NOT_CHAR_BOUNDARY);
    assert_code(b.apply(req(vec![ins(1, "x")]), &o("agent:a"), via(), 0), ErrorCode::InvalidArgument, reason::NOT_CHAR_BOUNDARY);
    assert_eq!(b.rev(), 0);
}

// ---- load / save -------------------------------------------------------------

#[test]
fn bom_and_eol_round_trip() {
    let input = b"\xef\xbb\xbfx\r\ny\r\n";
    let (b, meta) = Buffer::from_bytes(input).unwrap();
    assert!(meta.bom);
    assert_eq!(meta.eol, Eol::Crlf);
    assert_eq!(text(&b), "x\r\ny\r\n");
    assert_eq!(b.len(), 6);
    assert_eq!(b.to_bytes(&meta), input);
    for (s, eol) in [("a\nb", Eol::Lf), ("a\r\nb\n", Eol::Mixed), ("ab", Eol::None), ("", Eol::None)] {
        let (b, meta) = Buffer::from_bytes(s.as_bytes()).unwrap();
        assert_eq!(meta, crate::buffer::FileMeta { bom: false, eol }, "{s:?}");
        assert_eq!(b.to_bytes(&meta), s.as_bytes());
    }
}

#[test]
fn not_utf8() {
    let e = Buffer::from_bytes(&[b'a', 0xff]).err().unwrap();
    assert_eq!((e.code, e.reason), (ErrorCode::InvalidArgument, Some(reason::NOT_UTF8)));
    assert_eq!(e.context["offset"], 1);
}

#[test]
fn size_and_line_limits_at_load_and_edit() {
    assert_code(Buffer::from_bytes(&vec![b'a'; MAX_BUFFER_BYTES + 1]).map(|_| ()), ErrorCode::ResourceLimit, reason::TOO_LARGE);
    assert_code(Buffer::from_bytes("\n".repeat(MAX_LINES).as_bytes()).map(|_| ()), ErrorCode::ResourceLimit, reason::TOO_MANY_LINES);

    let mut b = buf(&"\n".repeat(MAX_LINES - 1));
    assert_eq!(b.line_count(), MAX_LINES);
    let before = b.fingerprint();
    assert_code(b.apply(req(vec![ins(0, "\n")]), &o("agent:a"), via(), 0), ErrorCode::ResourceLimit, reason::TOO_MANY_LINES);
    assert_eq!(b.fingerprint(), before);

    let mut b = buf(&"a".repeat(MAX_BUFFER_BYTES - 4));
    assert_code(b.apply(req(vec![ins(0, "0123456789")]), &o("agent:a"), via(), 0), ErrorCode::ResourceLimit, reason::TOO_LARGE);
    assert_eq!(b.len(), MAX_BUFFER_BYTES - 4);

    let mut b = buf("");
    let big = "x".repeat(MAX_REQUEST_TEXT_BYTES + 1);
    assert_code(b.apply(req(vec![ins(0, &big)]), &o("agent:a"), via(), 0), ErrorCode::ResourceLimit, reason::TOO_LARGE);
    let many = vec![ins(0, ""); MAX_OPS_PER_TXN + 1];
    assert_code(b.apply(req(many), &o("agent:a"), via(), 0), ErrorCode::ResourceLimit, reason::LIMIT);
}

// ---- transactions ----------------------------------------------------------

#[test]
fn overlap_in_txn() {
    let base = "0123456789abcdefghij";
    for ops in [
        vec![del(2, 6), ins(4, "x")],
        vec![del(2, 6), del(5, 8)],
        vec![del(2, 6), rep(2, 4, "y")],
        vec![rep(0, 20, "z"), del(19, 20)],
    ] {
        let mut b = buf(base);
        assert_code(b.apply(req(ops), &o("agent:a"), via(), 0), ErrorCode::InvalidArgument, reason::OVERLAP_IN_TXN);
    }
    let mut b = buf(base);
    edit(&mut b, "agent:a", vec![del(2, 6), ins(2, "x"), ins(6, "y")]);
    assert_eq!(text(&b), "01xy6789abcdefghij");
    let mut b = buf(base);
    edit(&mut b, "agent:a", vec![del(4, 6), del(2, 4)]);
    assert_eq!(text(&b), "016789abcdefghij");
}

#[test]
fn same_offset_inserts_read_in_request_order() {
    let mut b = buf("0123456789");
    let a = edit(&mut b, "agent:a", vec![ins(5, "A"), ins(5, "B"), ins(5, "C")]);
    assert_eq!(text(&b), "01234ABC56789");
    assert_eq!(a.changed, vec![5..6, 6..7, 7..8]);
}

#[test]
fn equal_start_application_order() {
    let mut b = buf("0123456789abcdefghijKLMN");
    let a = edit(&mut b, "agent:a", vec![del(10, 20), ins(10, "bar")]);
    assert_eq!(text(&b), "0123456789barKLMN");
    assert_eq!(
        a.edits,
        vec![Edit { offset: 10, delete: 10, insert: String::new() }, Edit { offset: 10, delete: 0, insert: "bar".into() }]
    );
    assert_eq!(a.changed, vec![10..13]);
    assert_eq!((a.inserted_bytes, a.deleted_bytes, a.base_rev, a.rev), (3, 10, 0, 1));

    let mut b = buf("0123456789abcdefghijKLMN");
    edit(&mut b, "agent:a", vec![rep(10, 20, "R"), ins(10, "A")]);
    assert_eq!(text(&b), "0123456789ARKLMN");
    let mut b = buf("0123456789abcdefghijKLMN");
    edit(&mut b, "agent:a", vec![ins(20, "E"), rep(10, 20, "R")]);
    assert_eq!(text(&b), "0123456789REKLMN");
}

#[test]
fn changed_is_computed_after_application() {
    let mut b = buf("abcdefgh");
    let a = edit(&mut b, "agent:a", vec![ins(0, "XX"), ins(5, "Y"), del(6, 7)]);
    assert_eq!(text(&b), "XXabcdeYfh");
    assert_eq!(a.changed, vec![0..2, 7..8]);
    assert_eq!(b.text().line_index(), recount(&text(&b)).as_slice());
}

#[test]
fn stale_cas_and_base_rev() {
    let mut b = buf("hello world");
    let mut r = req(vec![ins(0, "x")]);
    r.cas = Cas::ExpectRev(5);
    let e = b.apply(r, &o("agent:a"), via(), 0).err().unwrap();
    assert_eq!((e.code, e.reason), (ErrorCode::Conflict, Some(reason::STALE_REV)));
    assert_eq!(e.context["rev"], 0);

    edit(&mut b, "agent:a", vec![ins(0, ">> ")]);
    let mut r = req(vec![OpSpec::Insert { at: PosSpec::LineCol { line: 1, col: None }, text: "x".into() }]);
    r.cas = Cas::BaseRev(0);
    assert_code(b.apply(r, &o("agent:b"), via(), 0), ErrorCode::InvalidArgument, reason::BASE_REV_NEEDS_OFFSETS);

    let mut r = req(vec![rep(6, 11, "there")]);
    r.cas = Cas::BaseRev(0);
    let a = b.apply(r, &o("agent:b"), via(), 0).unwrap();
    assert!(a.rebased);
    assert_eq!(a.base_rev, 1);
    assert_eq!(text(&b), ">> hello there");

    edit(&mut b, "agent:a", vec![del(3, 6)]); // rev 3: ">> lo there"
    let mut r = req(vec![rep(4, 6, "Y")]); // rev-1 coords: inside the rev-3 delete
    r.cas = Cas::BaseRev(1);
    let e = b.apply(r, &o("agent:b"), via(), 0).err().unwrap();
    assert_eq!((e.code, e.reason), (ErrorCode::Conflict, Some(reason::OVERLAP)));
    assert_eq!(e.context["intervening_rev"], 3);
    assert_eq!(e.context["rev"], 3);

    let mut r = req(vec![ins(0, "x")]);
    r.cas = Cas::BaseRev(9);
    assert_code(b.apply(r, &o("agent:b"), via(), 0), ErrorCode::InvalidArgument, reason::BASE_REV_IN_FUTURE);

    let mut r = req(vec![ins(99, "x")]);
    r.cas = Cas::BaseRev(0);
    assert_code(b.apply(r, &o("agent:b"), via(), 0), ErrorCode::InvalidArgument, reason::OFFSET_OUT_OF_RANGE);
    assert_eq!(b.rev(), 3);
}

#[test]
fn op_id_is_recorded_and_echoed() {
    let mut b = buf("");
    let mut r = req(vec![ins(0, "x")]);
    r.op_id = Some("k-1".into());
    let a = b.apply(r, &o("agent:a"), via(), 7).unwrap();
    assert_eq!(a.op_id.as_deref(), Some("k-1"));
    let e = b.history(0, 10).next().unwrap();
    assert_eq!((e.op_id.as_deref(), e.time_ms, e.via.from.as_deref()), (Some("k-1"), 7, Some("test")));
}

#[test]
fn undo_records_its_op_id_and_previews_its_cost() {
    let mut b = buf("");
    let a = o("agent:a");
    b.apply(req(vec![ins(0, "hello")]), &a, via(), 0).unwrap();
    b.apply(req(vec![del(0, 5)]), &a, via(), 0).unwrap();
    // Undoing the delete restores 5 bytes of text and logs 5 inserted bytes.
    let before = b.fingerprint();
    assert_eq!(b.undo_cost(&LaneSel::Own, &a, false).unwrap(), 10);
    assert_eq!(b.fingerprint(), before, "the preview is pure");
    let u = b.undo_redo_op(LaneSel::Own, &a, via(), 0, false, Some("u-1".into())).unwrap();
    assert_eq!(u.op_id.as_deref(), Some("u-1"));
    assert_eq!(b.history(u.rev - 1, 1).next().unwrap().op_id.as_deref(), Some("u-1"));
    assert_eq!(text(&b), "hello");
    // Redo deletes again: no text growth, 5 logged deleted bytes.
    assert_eq!(b.undo_cost(&LaneSel::Own, &a, true).unwrap(), 5);
    assert_code(b.undo_cost(&LaneSel::Own, &o("agent:z"), false), ErrorCode::NotFound, reason::NOTHING_TO_UNDO);
}

// ---- lanes, coalescing, undo ------------------------------------------------

#[test]
fn coalescing_merges_a_typing_run() {
    let mut b = buf("");
    let a = o("agent:a");
    for (i, c) in ["a", "b", "c"].iter().enumerate() {
        b.apply(coalescing(vec![ins(i, c)]), &a, via(), 0).unwrap();
    }
    assert_eq!(b.lane_stacks(&a).0, vec![1..=3]);
    let u = b.undo(LaneSel::Own, &a, via(), 0).unwrap();
    assert_eq!(u.kind, EntryKind::Undo { of: 1..=3 });
    assert_eq!(text(&b), "");
    assert_eq!(b.lane_stacks(&a), (vec![], vec![4..=4]));
    let r = b.redo(LaneSel::Own, &a, via(), 0).unwrap();
    assert_eq!(r.kind, EntryKind::Redo { of: 4..=4 });
    assert_eq!(text(&b), "abc");
}

#[test]
fn coalescing_stops_at_another_origin() {
    let mut b = buf("");
    let (a, x) = (o("agent:a"), o("agent:x"));
    b.apply(coalescing(vec![ins(0, "a")]), &a, via(), 0).unwrap();
    b.apply(coalescing(vec![ins(1, "x")]), &x, via(), 0).unwrap();
    b.apply(coalescing(vec![ins(2, "b")]), &a, via(), 0).unwrap();
    assert_eq!(b.lane_stacks(&a).0, vec![1..=1, 3..=3]);
    // Non-contiguous typing also starts a new group.
    b.apply(coalescing(vec![ins(0, "q")]), &a, via(), 0).unwrap();
    assert_eq!(b.lane_stacks(&a).0.len(), 3);
}

#[test]
fn undo_runs_restore_original_order() {
    let a = o("agent:a");
    // Backspace run.
    let mut b = buf("abcdef");
    for s in [3, 2, 1] {
        b.apply(coalescing(vec![del(s, s + 1)]), &a, via(), 0).unwrap();
    }
    assert_eq!((text(&b).as_str(), b.lane_stacks(&a).0), ("aef", vec![1..=3]));
    b.undo(LaneSel::Own, &a, via(), 0).unwrap();
    assert_eq!(text(&b), "abcdef");
    // Forward-delete run (the plan's "newest member first" would give "acbdef").
    let mut b = buf("abcdef");
    for _ in 0..3 {
        b.apply(coalescing(vec![del(1, 2)]), &a, via(), 0).unwrap();
    }
    assert_eq!((text(&b).as_str(), b.lane_stacks(&a).0), ("aef", vec![1..=3]));
    b.undo(LaneSel::Own, &a, via(), 0).unwrap();
    assert_eq!(text(&b), "abcdef");
    // A forward-delete run with another origin's insert between the deletes' point.
    let mut b = buf("abcdef");
    b.apply(coalescing(vec![del(1, 2)]), &a, via(), 0).unwrap();
    b.apply(coalescing(vec![del(1, 2)]), &a, via(), 0).unwrap(); // "adef"
    edit(&mut b, "agent:x", vec![ins(0, "X")]); // "Xadef"
    b.undo(LaneSel::Own, &a, via(), 0).unwrap();
    assert_eq!(text(&b), "Xabcdef");
}

#[test]
fn linear_undo_through_an_undone_overlapping_edit() {
    // rev 2 rewrites rev 1's text; once rev 2 is undone, rev 1 must still undo,
    // even with another origin's edit in between (pair cancellation).
    let a = o("agent:a");
    let mut b = buf("hello world");
    edit(&mut b, "agent:a", vec![ins(5, " big")]);
    edit(&mut b, "agent:a", vec![rep(6, 9, "huge")]);
    edit(&mut b, "agent:x", vec![ins(0, ">")]);
    b.undo(LaneSel::Own, &a, via(), 0).unwrap();
    assert_eq!(text(&b), ">hello big world");
    b.undo(LaneSel::Own, &a, via(), 0).unwrap();
    assert_eq!(text(&b), ">hello world");
    b.redo(LaneSel::Own, &a, via(), 0).unwrap();
    assert_eq!(text(&b), ">hello big world");
    b.redo(LaneSel::Own, &a, via(), 0).unwrap();
    assert_eq!(text(&b), ">hello huge world");
    b.undo(LaneSel::Own, &a, via(), 0).unwrap();
    b.undo(LaneSel::Own, &a, via(), 0).unwrap();
    assert_eq!(text(&b), ">hello world");
}

#[test]
fn undo_own_other_lane_and_global() {
    let mut b = buf("0123456789");
    let (a, x) = (o("agent:a"), o("agent:x"));
    edit(&mut b, "agent:a", vec![ins(0, "A")]);
    edit(&mut b, "agent:x", vec![ins(11, "X")]);
    assert_eq!(text(&b), "A0123456789X");
    b.undo(LaneSel::Own, &a, via(), 0).unwrap();
    assert_eq!(text(&b), "0123456789X");
    let u = b.undo(LaneSel::Lane(x.clone()), &a, via(), 0).unwrap();
    assert_eq!(text(&b), "0123456789");
    let entry = b.history(u.rev - 1, 1).next().unwrap();
    assert_eq!((&entry.origin, &entry.lane), (&a, &x));
    // Global redo picks the lane whose top redo group is newest (x's, rev 4).
    b.redo(LaneSel::All, &a, via(), 0).unwrap();
    assert_eq!(text(&b), "0123456789X");
    b.undo(LaneSel::All, &a, via(), 0).unwrap();
    assert_eq!(text(&b), "0123456789");
    assert_code(b.undo(LaneSel::Lane(x.clone()), &a, via(), 0), ErrorCode::NotFound, reason::NOTHING_TO_UNDO);
    assert_code(buf("").undo(LaneSel::All, &a, via(), 0), ErrorCode::NotFound, reason::NOTHING_TO_UNDO);
    assert_code(buf("").redo(LaneSel::Own, &a, via(), 0), ErrorCode::NotFound, reason::NOTHING_TO_REDO);
}

#[test]
fn failed_undo_preflight_changes_nothing() {
    let mut b = buf("xyz");
    let (a, x) = (o("agent:a"), o("agent:x"));
    b.apply(coalescing(vec![ins(0, "a")]), &a, via(), 0).unwrap();
    b.apply(coalescing(vec![ins(1, "b")]), &a, via(), 0).unwrap(); // "abxyz", group 1..=2
    b.anchor_set("k", AnchorSpec { at: Some(PosSpec::Offset(3)), ..Default::default() }).unwrap();
    b.anchor_set("r", AnchorSpec { range: Some(RangeSpec::Offsets([4, 5])), ..Default::default() }).unwrap();
    edit(&mut b, "agent:x", vec![del(2, 4)]); // "abz": anchor k collapses at rev 3
    assert_eq!(b.anchor("k").unwrap().start.collapsed_rev, Some(3));
    b.set_selections(&a, vec![Selection { anchor: 1, head: 2 }]).unwrap();
    b.set_selections(&x, vec![Selection { anchor: 3, head: 3 }]).unwrap();
    edit(&mut b, "agent:x", vec![del(0, 1)]); // "bz": deletes the OLDER member's text
    let before = b.fingerprint();
    let e = b.undo(LaneSel::Own, &a, via(), 0).err().unwrap();
    assert_eq!((e.code, e.reason), (ErrorCode::Conflict, Some(reason::UNDO_CONFLICT)));
    assert_eq!(e.context["intervening_rev"], 4);
    assert_eq!(e.context["intervening_origin"], "agent:x");
    assert_eq!(e.context["lane"], "agent:a");
    assert_eq!(b.fingerprint(), before);
    // The other lane still undoes.
    b.undo(LaneSel::Own, &x, via(), 0).unwrap();
    assert_eq!(text(&b), "abz");
}

#[test]
fn retention_trims_and_drops_unreachable_groups() {
    let mut b = buf("");
    let a = o("agent:a");
    let mb = "a".repeat(1024 * 1024);
    let mut trimmed = None;
    for _ in 0..40 {
        let r = b.apply(req(vec![ins(0, &mb)]), &a, via(), 0).unwrap();
        trimmed = r.history_trimmed_to.or(trimmed);
    }
    assert_eq!(trimmed, Some(8));
    assert_eq!(b.oldest_rev(), 8);
    assert_eq!(b.history(0, 1000).count(), 32);
    let undo = b.lane_stacks(&a).0;
    assert_eq!((undo.len(), undo[0].clone()), (32, 9..=9));
    let mut r = req(vec![ins(0, "x")]);
    r.cas = Cas::BaseRev(3);
    let e = b.apply(r, &a, via(), 0).err().unwrap();
    assert_eq!((e.code, e.reason), (ErrorCode::Conflict, Some(reason::HISTORY_TRIMMED)));
    assert_eq!(e.context["oldest_rev"], 8);
    assert!(b.log_text_bytes() <= crate::limits::LOG_MAX_TEXT_BYTES);
}

#[test]
fn reload_minimal_keeps_anchors_outside_the_change() {
    let mut b = buf("line1\nline2\nline3\n");
    for (n, at) in [("top", 0), ("mid", 8), ("bot", 12)] {
        b.anchor_set(n, AnchorSpec { at: Some(PosSpec::Offset(at)), ..Default::default() }).unwrap();
    }
    assert!(b.reload_minimal("line1\nline2\nline3\n", via(), 0).unwrap().is_none());
    assert_eq!(b.rev(), 0);
    let r = b.reload_minimal("line1\nLINE2\nline3\n", via(), 0).unwrap().unwrap();
    assert_eq!(r.kind, EntryKind::Reload);
    assert_eq!(r.edits, vec![Edit { offset: 6, delete: 4, insert: "LINE".into() }]);
    assert_eq!(text(&b), "line1\nLINE2\nline3\n");
    assert_eq!(b.anchor("top").unwrap().start.offset, 0);
    assert_eq!(b.anchor("bot").unwrap().start.offset, 12);
    assert_eq!(b.anchor("mid").unwrap().start.collapsed_rev, Some(1));
    let disk = o("tool:disk");
    assert_eq!(b.history(0, 1).next().unwrap().lane, disk);
    b.undo(LaneSel::Lane(disk), &o("human:ced"), via(), 0).unwrap();
    assert_eq!(text(&b), "line1\nline2\nline3\n");
    // Multibyte prefix/suffix stay on char boundaries.
    let mut b = buf("aé€b");
    b.reload_minimal("aè€b", via(), 0).unwrap().unwrap();
    assert_eq!(text(&b), "aè€b");
}

// ---- anchors, selections, cursor ------------------------------------------

#[test]
fn anchor_bias_ranges_and_collapse() {
    let mut b = buf("0123456789");
    b.anchor_set("before", AnchorSpec { at: Some(PosSpec::Offset(5)), ..Default::default() }).unwrap();
    b.anchor_set("after", AnchorSpec { at: Some(PosSpec::Offset(5)), bias: Some(Bias::After), ..Default::default() })
        .unwrap();
    b.anchor_set("range", AnchorSpec { range: Some(RangeSpec::Offsets([2, 5])), ..Default::default() }).unwrap();
    edit(&mut b, "agent:a", vec![ins(5, "XY")]);
    assert_eq!(b.anchor("before").unwrap().start.offset, 5);
    assert_eq!(b.anchor("after").unwrap().start.offset, 7);
    let r = b.anchor("range").unwrap();
    assert_eq!((r.start.offset, r.end.unwrap().offset), (2, 5)); // non-expanding
    edit(&mut b, "agent:a", vec![ins(2, "Z")]); // at the range start: stays outside
    let r = b.anchor("range").unwrap();
    assert_eq!((r.start.offset, r.end.unwrap().offset), (3, 6));
    edit(&mut b, "agent:a", vec![del(1, 8)]); // swallows everything
    let r = b.anchor("range").unwrap();
    assert_eq!((r.start.offset, r.end.unwrap().offset), (1, 1));
    assert_eq!(r.start.collapsed_rev, Some(3));
    assert_eq!(b.anchor("before").unwrap().start.collapsed_rev, Some(3));
    assert!(b.anchor_clear("before"));
    assert!(!b.anchor_clear("before"));
    assert_code(b.anchor_set("bad name", AnchorSpec::default()), ErrorCode::InvalidArgument, reason::BAD_NAME);
    assert_code(b.anchor_set("x", AnchorSpec::default()), ErrorCode::InvalidArgument, reason::BAD_ARGS);
    // An anchor can be a position.
    b.anchor_set("m", AnchorSpec { at: Some(PosSpec::Offset(2)), ..Default::default() }).unwrap();
    edit(&mut b, "agent:a", vec![OpSpec::Insert { at: PosSpec::Anchor { anchor: "m".into() }, text: "!".into() }]);
    assert_eq!(&text(&b)[2..3], "!");
}

#[test]
fn selections_map_by_origin_and_cursor_is_post_edit() {
    let mut b = buf("0123456789");
    let (a, x) = (o("human:ced"), o("agent:x"));
    b.set_selections(&a, vec![Selection { anchor: 5, head: 5 }]).unwrap();
    b.set_selections(&x, vec![Selection { anchor: 5, head: 7 }]).unwrap();
    b.apply(req(vec![ins(5, "ab")]), &a, via(), 0).unwrap();
    let sels: Vec<_> = b.selections().map(|(o, s)| (o.clone(), s.to_vec())).collect();
    // BTreeMap order: human < agent.
    assert_eq!(sels[0], (a.clone(), vec![Selection { anchor: 7, head: 7 }])); // editor: After
    assert_eq!(sels[1], (x.clone(), vec![Selection { anchor: 5, head: 9 }])); // others: Before

    let mut r = req(vec![ins(0, "L1\n")]);
    r.cursor = Some(PosSpec::LineCol { line: 2, col: Some(3) });
    b.apply(r, &a, via(), 0).unwrap();
    assert_eq!(b.selections().find(|(o, _)| **o == a).unwrap().1, &[Selection { anchor: 5, head: 5 }]);

    let mut r = req(vec![ins(0, "é")]);
    r.cursor = Some(PosSpec::Offset(1));
    let before = b.fingerprint();
    assert_code(b.apply(r, &a, via(), 0), ErrorCode::InvalidArgument, reason::NOT_CHAR_BOUNDARY);
    assert_eq!(b.fingerprint(), before);
    let mut r = req(vec![ins(0, "x\n")]);
    r.cursor = Some(PosSpec::LineCol { line: 1, col: Some(3) });
    assert_code(b.apply(r, &a, via(), 0), ErrorCode::InvalidArgument, reason::COL_OUT_OF_RANGE);

    assert_code(b.set_selections(&a, vec![Selection { anchor: 0, head: 99 }]), ErrorCode::InvalidArgument, reason::OFFSET_OUT_OF_RANGE);
    assert_code(b.set_selections(&a, vec![Selection { anchor: 0, head: 0 }; 17]), ErrorCode::ResourceLimit, reason::LIMIT);
    b.set_selections(&a, vec![]).unwrap();
    assert_eq!(b.selections().count(), 1);
}

// ---- search ------------------------------------------------------------------

fn q(pattern: &str) -> FindQuery {
    FindQuery { pattern: pattern.into(), regex: false, case: true, range: None, groups: false, limit: 1000, from: None }
}

#[test]
fn find_literal_regex_case_and_paging() {
    let mut b = buf("Alpha beta\nalpha gamma\nALPHA");
    let r = b.find(&q("alpha"), 1 << 20).unwrap();
    assert_eq!(r.matches.iter().map(|m| m.range.clone()).collect::<Vec<_>>(), vec![11..16]);
    let r = b.find(&FindQuery { case: false, ..q("alpha") }, 1 << 20).unwrap();
    assert_eq!(r.matches.len(), 3);
    assert!(!r.truncated && r.next.is_none());
    let r = b.find(&FindQuery { regex: true, ..q(r"^a\w+") }, 1 << 20).unwrap();
    assert_eq!(r.matches[0].text, "alpha");
    let r = b.find(&FindQuery { case: false, limit: 2, ..q("alpha") }, 1 << 20).unwrap();
    assert_eq!((r.matches.len(), r.truncated, r.next), (2, true, Some(16)));
    let r = b.find(&FindQuery { case: false, from: Some(16), ..q("alpha") }, 1 << 20).unwrap();
    assert_eq!(r.matches.len(), 1);
    let r = b.find(&FindQuery { case: false, ..q("alpha") }, 1).unwrap(); // budget: one match always fits
    assert_eq!((r.matches.len(), r.truncated, r.next), (1, true, Some(5)));
    let r = b.find(&FindQuery { case: false, range: Some(RangeSpec::Lines { lines: [2, 2] }), ..q("alpha") }, 1 << 20)
        .unwrap();
    assert_eq!(r.matches.len(), 1);
    assert_code(b.find(&FindQuery { regex: true, ..q("(") }, 1 << 20), ErrorCode::InvalidArgument, reason::BAD_REGEX);
    // The search moved the gap; reads and edits still agree.
    edit(&mut b, "agent:a", vec![ins(0, "!")]);
    assert_eq!(text(&b), "!Alpha beta\nalpha gamma\nALPHA");
}

#[test]
fn find_groups_and_empty_matches() {
    let mut b = buf("ab-a");
    let r = b.find(&FindQuery { regex: true, groups: true, ..q("(a)(b)?(z)?") }, 1 << 20).unwrap();
    assert_eq!(r.matches[0].groups, Some(vec![Some("a".into()), Some("b".into()), None]));
    assert_eq!(r.matches[1].groups, Some(vec![Some("a".into()), None, None]));
    let mut b = buf("aab");
    let r = b.find(&FindQuery { regex: true, ..q("a*") }, 1 << 20).unwrap();
    assert_eq!(r.matches.iter().map(|m| m.range.clone()).collect::<Vec<_>>(), vec![0..2, 3..3]);
    let r = b.find(&FindQuery { regex: true, limit: 1, ..q("x*") }, 1 << 20).unwrap();
    assert_eq!((r.matches[0].range.clone(), r.next), (0..0, Some(1)));
}

#[test]
fn find_caps_one_match_and_pages_advance() {
    let a = "a".repeat(70_000);
    let mut b = buf(&format!("{a}b{a}"));
    let pattern = format!("{}a+{}", "(".repeat(16), ")".repeat(16));
    let fq = FindQuery { regex: true, groups: true, ..q(&pattern) };
    let r = b.find(&fq, 100 * 1024).unwrap();
    let m = &r.matches[0];
    assert_eq!(m.range, 0..70_000);
    assert!(m.text_truncated && m.text.len() <= crate::limits::MATCH_TEXT_MAX);
    assert!(m.groups_truncated);
    let groups = m.groups.as_ref().unwrap();
    // Cut at the budget: the groups that fit, none after (not nulls).
    assert!(!groups.is_empty() && groups.len() < 16, "{} groups", groups.len());
    assert!(groups.iter().all(Option::is_some));
    assert_eq!((r.matches.len(), r.truncated, r.next), (1, true, Some(70_000)));
    let r = b.find(&FindQuery { from: r.next, ..fq }, 100 * 1024).unwrap();
    assert_eq!(r.matches[0].range, 70_001..140_001);
    assert!(!r.truncated);
}

#[test]
fn many_empty_groups_stay_under_the_match_cap() {
    // 30,000 empty captures: 3 encoded bytes each is past MATCH_ENCODED_MAX,
    // and a `null` per omitted slot would keep growing the match past it.
    let mut b = buf("x");
    let fq = FindQuery { regex: true, groups: true, limit: 1, ..q(&"()".repeat(30_000)) };
    let r = b.find(&fq, 1 << 20).unwrap();
    let m = &r.matches[0];
    assert!(m.groups_truncated);
    let encoded = serde_json::to_string(m.groups.as_ref().unwrap()).unwrap().len();
    assert!(encoded <= crate::limits::MATCH_ENCODED_MAX, "groups encode to {encoded} bytes");
}

// ---- language ---------------------------------------------------------------

#[test]
fn language_table() {
    let d = |p: &str, first: &str| detect(Some(Path::new(p)), first);
    assert_eq!(d("/x/scene.mix", ""), "scene");
    assert_eq!(d("/x/node.conf.mix", ""), "mix-data");
    assert_eq!(d("/x/a.mix", ""), "mix");
    assert_eq!(d("/x/tool", "#!/opt/cosmix/bin/mix"), "mix");
    assert_eq!(d("/x/tool", "#!/usr/bin/env mix"), "mix");
    assert_eq!(d("/x/tool.sh", "#!/usr/bin/env -S mix --quiet"), "mix");
    assert_eq!(d("/x/tool.sh", "#!/bin/sh"), "shell");
    for (p, l) in [
        ("a.rs", "rust"),
        ("README.md", "markdown"),
        ("Cargo.toml", "toml"),
        ("a.json", "json"),
        ("a.yml", "yaml"),
        ("a.zsh", "shell"),
        ("a.py", "python"),
        ("a.mjs", "javascript"),
        ("a.h", "c"),
        ("a.cc", "cpp"),
        ("a.go", "go"),
        ("a.lua", "lua"),
        ("a.svg", "xml"),
        ("a.patch", "diff"),
        ("COMMIT_EDITMSG", "git_commit"),
        ("a.ts", "text"),
        (".bashrc", "text"),
        ("Makefile", "text"),
    ] {
        assert_eq!(d(p, ""), l, "{p}");
    }
    assert_eq!(detect(None, ""), "text");
    assert_eq!(detect(None, "#!/opt/cosmix/bin/mix"), "mix");
}

// ---- OT ------------------------------------------------------------------------

#[test]
fn transform_rules_and_inverse_example() {
    let e = |offset, delete, insert: &str| Edit { offset, delete, insert: insert.into() };
    let t = |r, ed: &Edit, p| transform_range(r, ed, p);
    assert_eq!(t(5..8, &e(1, 2, "xyz"), Priority::ThroughFirst), Ok(6..9));
    assert_eq!(t(5..8, &e(3, 2, ""), Priority::ThroughFirst), Ok(3..6)); // delete ends at s
    assert_eq!(t(5..8, &e(8, 2, "q"), Priority::ThroughFirst), Ok(5..8)); // delete starts at e
    assert_eq!(t(5..5, &e(5, 0, "ab"), Priority::ThroughFirst), Ok(7..7));
    assert_eq!(t(5..5, &e(5, 0, "ab"), Priority::SelfFirst), Ok(5..5));
    assert_eq!(t(5..8, &e(5, 0, "ab"), Priority::SelfFirst), Ok(7..10));
    assert_eq!(t(5..8, &e(8, 0, "ab"), Priority::ThroughFirst), Ok(5..8));
    assert!(t(5..8, &e(6, 0, "ab"), Priority::ThroughFirst).is_err());
    assert!(t(5..8, &e(4, 2, ""), Priority::ThroughFirst).is_err());
    assert!(t(5..5, &e(4, 2, ""), Priority::ThroughFirst).is_err());
    let set = RangeSet { rev: 3, items: vec![(0..1, "a".into()), (4..4, "b".into())] };
    let moved = transform_set(&set, &[e(2, 0, "zz")], Priority::ThroughFirst).unwrap();
    assert_eq!(moved.items, vec![(0..1, "a".into()), (6..6, "b".into())]);
    assert_eq!(moved.rev, 3);

    // The frozen worked example: abcd + [X@3, Y@0] → YabcXd; inverse {[0,1)→"", [4,5)→""}.
    let mut b = buf("abcd");
    edit(&mut b, "agent:a", vec![ins(3, "X"), ins(0, "Y")]);
    assert_eq!(text(&b), "YabcXd");
    let inv = invert(b.history(0, 1).next().unwrap());
    assert_eq!(inv, RangeSet { rev: 1, items: vec![(0..1, String::new()), (4..5, String::new())] });
}

// ---- failure injection (plan §3.1, §6.1) ----------------------------------------

fn with_failing_commits<T>(f: impl FnOnce() -> T) -> T {
    inject_commit_failure(true);
    let out = f();
    inject_commit_failure(false);
    out
}

fn peak_base() -> String {
    "0123456789\n".repeat(30_000)
}

#[test]
fn failed_commit_refuses_single_and_multi_op_txns_untouched() {
    for ops in [
        vec![ins(0, &"x".repeat(200 * 1024))],
        vec![ins(0, &"x".repeat(100 * 1024)), ins(5, &"y\n".repeat(50 * 1024))],
    ] {
        let mut b = buf("hello");
        b.set_selections(&o("agent:a"), vec![Selection { anchor: 1, head: 2 }]).unwrap();
        b.anchor_set("m", AnchorSpec { at: Some(PosSpec::Offset(3)), ..Default::default() }).unwrap();
        let before = b.fingerprint();
        let r = with_failing_commits(|| b.apply(req(ops.clone()), &o("agent:a"), via(), 0));
        assert_code(r, ErrorCode::ResourceLimit, reason::OUT_OF_MEMORY);
        assert_eq!(b.fingerprint(), before);
        b.apply(req(ops), &o("agent:a"), via(), 0).unwrap();
        assert_eq!(b.text().line_index(), recount(&text(&b)).as_slice());
    }
}

#[test]
fn peak_case_is_simulated_and_refused_whole() {
    let base = peak_base();
    let big = "x\n".repeat(400_000);
    let ops = vec![ins(base.len(), &big), del(0, 11 * 27_000)];

    let mut b = buf(&base);
    let before = b.fingerprint();
    let r = with_failing_commits(|| b.apply(req(ops.clone()), &o("agent:a"), via(), 0));
    assert_code(r, ErrorCode::ResourceLimit, reason::OUT_OF_MEMORY);
    assert_eq!(b.fingerprint(), before);

    b.apply(req(ops), &o("agent:a"), via(), 0).unwrap();
    let model = format!("{}{big}", &base[11 * 27_000..]);
    assert_eq!(text(&b), model);
    assert_eq!(b.text().line_index(), recount(&model).as_slice());
}

#[test]
fn phase_two_neither_commits_nor_allocates() {
    let base = peak_base();
    let mut t = Text::from_text(&base).unwrap();
    let seq = vec![
        Edit { offset: base.len(), delete: 0, insert: "x\n".repeat(400_000) },
        Edit { offset: 0, delete: 11 * 27_000, insert: "head\n".into() },
    ];
    let p = t.prepare(seq).unwrap();
    assert_eq!(p.peak_len, base.len() + 800_000);
    assert_eq!(p.peak_lines, 30_001 + 400_000);
    assert_eq!(p.final_len, base.len() + 800_000 - 297_000 + 5);
    assert_eq!(p.final_lines, 30_001 + 400_000 - 27_000 + 1);
    let (calls, cap) = (t.commit_calls(), t.line_index_capacity());
    t.commit(p);
    assert_eq!(t.commit_calls(), calls, "phase 2 committed memory");
    assert_eq!(t.line_index_capacity(), cap, "phase 2 reallocated the line index");
    let model = format!("head\n{}{}", &base[297_000..], "x\n".repeat(400_000));
    assert_eq!(t.to_string_lossless(), model);
    assert_eq!(t.line_index(), recount(&model).as_slice());
}

#[test]
fn net_line_deleting_txn_has_no_underflow() {
    let base: String = (0..100).map(|i| format!("line {i}\n")).collect();
    let mut b = buf(&base);
    let s = b.resolve_pos(&PosSpec::LineCol { line: 10, col: None }).unwrap();
    let e = b.resolve_pos(&PosSpec::LineCol { line: 90, col: None }).unwrap();
    let ops = vec![del(s, e), ins(base.len(), &"z".repeat(200 * 1024))];
    let before = b.fingerprint();
    let r = with_failing_commits(|| b.apply(req(ops.clone()), &o("agent:a"), via(), 0));
    assert_code(r, ErrorCode::ResourceLimit, reason::OUT_OF_MEMORY);
    assert_eq!(b.fingerprint(), before);
    b.apply(req(ops), &o("agent:a"), via(), 0).unwrap();
    assert_eq!(b.line_count(), 21);
    assert_eq!(b.text().line_index(), recount(&text(&b)).as_slice());
    b.undo(LaneSel::Own, &o("agent:a"), via(), 0).unwrap();
    assert_eq!(text(&b), base);
}

#[test]
fn failed_undo_commit_changes_nothing() {
    // Loaded at 300 KiB (commit ~384 KiB); x deletes it all, y types 200 KiB:
    // undoing x needs ~500 KiB, beyond what is committed.
    let base = "q".repeat(300 * 1024);
    let mut b = buf(&base);
    let (x, y) = (o("agent:x"), o("agent:y"));
    b.apply(req(vec![del(0, base.len())]), &x, via(), 0).unwrap();
    b.apply(req(vec![ins(0, &"y".repeat(200 * 1024))]), &y, via(), 0).unwrap();
    b.anchor_set("m", AnchorSpec { at: Some(PosSpec::Offset(100)), ..Default::default() }).unwrap();
    b.set_selections(&y, vec![Selection { anchor: 3, head: 9 }]).unwrap();
    let before = b.fingerprint();
    let r = with_failing_commits(|| b.undo(LaneSel::Own, &x, via(), 0));
    assert_code(r, ErrorCode::ResourceLimit, reason::OUT_OF_MEMORY);
    assert_eq!(b.fingerprint(), before);
    b.undo(LaneSel::Own, &x, via(), 0).unwrap();
    assert_eq!(text(&b), format!("{}{base}", "y".repeat(200 * 1024)));
}

#[test]
fn non_canonical_sequences_are_refused_by_prepare() {
    let mut t = Text::from_text("abcdef").unwrap();
    let seq = vec![Edit { offset: 0, delete: 1, insert: "x".into() }, Edit { offset: 3, delete: 1, insert: "y".into() }];
    let e = t.prepare(seq).err().unwrap();
    assert_eq!(e.code, ErrorCode::Internal);
    assert_eq!(t.to_string_lossless(), "abcdef");
}
