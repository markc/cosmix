//! Property tests (plan §6.1, proptest 1-8). Models are plain `String`s and
//! naive recomputation; nothing here reuses the code under test's arithmetic.

use proptest::prelude::*;

use super::*;
use crate::anchor::{AnchorSpec, Bias};
use crate::buffer::LaneSel;
use crate::ot::{Edit, Priority, invert, transform_range};
use crate::vendor::msedit::document::ReadableDocument;
use crate::vendor::msedit::gap_buffer::GapBuffer;

/// Text over an alphabet with multibyte scalars, `\r\n` and bare `\r`.
fn text_strategy(max: usize) -> impl Strategy<Value = String> {
    prop::collection::vec(prop::sample::select(vec!["a", "b", "\n", "\r\n", "\r", "é", "€", "😀"]), 0..max)
        .prop_map(|v| v.concat())
}

fn insert_strategy() -> impl Strategy<Value = String> {
    prop::collection::vec(prop::sample::select(vec!["x", "y", "\n", "\r\n", "ü", "😀"]), 0..4).prop_map(|v| v.concat())
}

/// Raw op: kind (0 insert, 1 delete, 2 replace), two boundary picks, text.
type RawOp = (u8, usize, usize, String);

fn raw_ops(max: usize) -> impl Strategy<Value = Vec<RawOp>> {
    prop::collection::vec((0u8..3, any::<usize>(), any::<usize>(), insert_strategy()), 1..max)
}

fn boundaries(s: &str) -> Vec<usize> {
    (0..=s.len()).filter(|&i| s.is_char_boundary(i)).collect()
}

/// Maps raw picks onto the char boundaries of `base`: `(s, e, text)` plus the op.
fn realise(base: &str, raw: &[RawOp]) -> Vec<(usize, usize, String, OpSpec)> {
    let b = boundaries(base);
    raw.iter()
        .map(|(kind, x, y, t)| {
            let (p, q) = (b[x % b.len()], b[y % b.len()]);
            let (s, e) = (p.min(q), p.max(q));
            match kind {
                0 => (p, p, t.clone(), ins(p, t)),
                1 => (s, e, String::new(), del(s, e)),
                _ => (s, e, t.clone(), rep(s, e, t)),
            }
        })
        .collect()
}

/// The §3.4 overlap rule, restated naively.
fn overlaps(ops: &[(usize, usize, String, OpSpec)]) -> bool {
    for (i, a) in ops.iter().enumerate() {
        for b in &ops[i + 1..] {
            let (ane, bne) = (a.1 > a.0, b.1 > b.0);
            if ane && bne && a.0.max(b.0) < a.1.min(b.1) {
                return true;
            }
            if !ane && bne && b.0 < a.0 && a.0 < b.1 {
                return true;
            }
            if ane && !bne && a.0 < b.0 && b.0 < a.1 {
                return true;
            }
        }
    }
    false
}

/// The largest prefix-greedy subset of `ops` that does not overlap.
fn non_overlapping(ops: Vec<(usize, usize, String, OpSpec)>) -> Vec<(usize, usize, String, OpSpec)> {
    let mut kept: Vec<(usize, usize, String, OpSpec)> = Vec::new();
    for op in ops {
        kept.push(op);
        if overlaps(&kept) {
            kept.pop();
        }
    }
    kept
}

/// Base-coordinate model: at each start, pure inserts in request order, then
/// the range op's replacement. Returns the text and the inserted spans.
fn model_apply(base: &str, ops: &[(usize, usize, String, OpSpec)]) -> (String, Vec<std::ops::Range<usize>>) {
    let mut order: Vec<usize> = (0..ops.len()).collect();
    order.sort_by_key(|&i| (ops[i].0, ops[i].1 > ops[i].0, i));
    let mut out = String::new();
    let mut spans = Vec::new();
    let mut pos = 0;
    for i in order {
        let (s, e, t, _) = &ops[i];
        out.push_str(&base[pos..*s]);
        pos = *s;
        if !t.is_empty() {
            spans.push(out.len()..out.len() + t.len());
        }
        out.push_str(t);
        pos = pos.max(*e);
    }
    out.push_str(&base[pos..]);
    spans.sort_by_key(|r| r.start);
    (out, spans)
}

/// Char-tagging model for §3.6: the text is a vector of cells, the anchor a
/// marker cell between them. Each edit removes the deleted byte cells, then
/// places the inserted cells at the edit point: behind a marker that sat at
/// the edit point with `Before` bias or was swallowed by the delete, ahead of
/// one with `After` bias or one that followed the deleted span.
fn marker_model(base: &str, at: usize, bias: Bias, edits: &[Edit]) -> usize {
    #[derive(Clone, Copy, PartialEq)]
    enum Cell {
        Byte,
        Marker,
    }
    let mut cells = vec![Cell::Byte; base.len()];
    cells.insert(at, Cell::Marker);
    for e in edits {
        let m = cells.iter().position(|c| *c == Cell::Marker).unwrap(); // bytes before the marker
        let (p, end) = (e.offset, e.offset + e.delete);
        let marker_first = if m < p || m > end {
            None // not at the edit point: position only shifts
        } else if m == p {
            Some(bias == Bias::Before)
        } else if m < end {
            Some(true) // swallowed: collapses to the point, Before
        } else {
            Some(false) // followed the deleted span: stays after its replacement
        };
        // Remove the deleted bytes and the marker, then rebuild around p.
        let bytes_before = cells.len() - 1 - e.delete;
        let mut out = vec![Cell::Byte; p];
        let ins = vec![Cell::Byte; e.insert.len()];
        match marker_first {
            Some(true) => {
                out.push(Cell::Marker);
                out.extend(&ins);
            }
            Some(false) => {
                out.extend(&ins);
                out.push(Cell::Marker);
            }
            None => out.extend(&ins),
        }
        out.extend(vec![Cell::Byte; bytes_before - p]);
        if marker_first.is_none() {
            let new_m = if m < p { m } else { m - e.delete + e.insert.len() };
            out.retain(|c| *c == Cell::Byte);
            out.insert(new_m, Cell::Marker);
        }
        cells = out;
    }
    cells.iter().position(|c| *c == Cell::Marker).unwrap()
}

fn apply_str(s: &str, r: std::ops::Range<usize>, t: &str) -> String {
    format!("{}{t}{}", &s[..r.start], &s[r.end..])
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// 1. Text vs model, incl. same-start batches; line index vs recount; `changed`.
    #[test]
    fn text_matches_base_coordinate_model(base in text_strategy(24), raw in raw_ops(6)) {
        let ops = realise(&base, &raw);
        let mut b = buf(&base);
        let r = b.apply(req(ops.iter().map(|o| o.3.clone()).collect()), &o("agent:a"), via(), 0);
        if overlaps(&ops) {
            let e = r.err().expect("overlapping ops must be refused");
            prop_assert_eq!(e.reason, Some(crate::error::reason::OVERLAP_IN_TXN));
            prop_assert_eq!(text(&b), base);
            return Ok(());
        }
        let applied = r.unwrap();
        let (model, spans) = model_apply(&base, &ops);
        prop_assert_eq!(text(&b), model.clone());
        prop_assert_eq!(b.text().line_index().to_vec(), recount(&model));
        prop_assert_eq!(applied.changed, spans);
    }

    /// 2. Point round trip at every char boundary, CRLF interiors included.
    #[test]
    fn point_round_trips(base in text_strategy(40)) {
        let b = buf(&base);
        for off in boundaries(&base) {
            let p = b.point(off);
            prop_assert_eq!(p.offset, off);
            let back = b.resolve_pos(&PosSpec::LineCol { line: p.line, col: Some(p.col) }).unwrap();
            prop_assert_eq!(back, off);
        }
        prop_assert_eq!(b.line_count(), base.matches('\n').count() + 1);
    }

    /// 3. A random txn then undo(Own) restores the text (twice, with redo between).
    #[test]
    fn undo_inverts_any_txn(base in text_strategy(24), raw1 in raw_ops(6), raw2 in raw_ops(4)) {
        let a = o("agent:a");
        let mut b = buf(&base);
        let mut states = vec![base.clone()];
        for raw in [raw1, raw2] {
            let cur = text(&b);
            let ops = non_overlapping(realise(&cur, &raw));
            b.apply(req(ops.iter().map(|o| o.3.clone()).collect()), &a, via(), 0).unwrap();
            states.push(text(&b));
        }
        let top = states.pop().unwrap();
        while let Some(prev) = states.pop() {
            b.undo(LaneSel::Own, &a, via(), 0).unwrap();
            prop_assert_eq!(text(&b), prev);
        }
        while b.redo(LaneSel::Own, &a, via(), 0).is_ok() {}
        prop_assert_eq!(text(&b), top);
        prop_assert_eq!(b.text().line_index().to_vec(), recount(&text(&b)));
    }

    /// 4. Lane independence: A edits even cells, B odd cells, interleaved;
    ///    undoing all of A leaves exactly B's edits.
    #[test]
    fn lanes_are_independent(
        steps in prop::collection::vec((any::<bool>(), 0usize..3, insert_strategy(), any::<bool>()), 1..12)
    ) {
        let (a, bo) = (o("agent:a"), o("agent:b"));
        let init: Vec<String> = (0..6).map(|i| format!("c{i}")).collect();
        let mut b = buf(&init.join("|"));
        let mut only_b = init.clone();
        for (is_a, slot, content, coalesce) in steps {
            let cell = if is_a { slot * 2 } else { slot * 2 + 1 };
            let cur = text(&b);
            let start: usize = cur.split('|').take(cell).map(|c| c.len() + 1).sum();
            let end = start + cur.split('|').nth(cell).unwrap().len();
            let r = TxnRequest { coalesce, ..req(vec![rep(start, end, &content)]) };
            b.apply(r, if is_a { &a } else { &bo }, via(), 0).unwrap();
            if !is_a {
                only_b[cell] = content;
            }
        }
        while b.undo(LaneSel::Own, &a, via(), 0).is_ok() {}
        prop_assert_eq!(text(&b), only_b.join("|"));
    }

    /// 5. OT convergence, both priorities: server `Y; X↑ThroughFirst` equals
    ///    client `X; Y↑SelfFirst`, and the buffer's base_rev path agrees.
    #[test]
    fn ot_converges(base in text_strategy(16), raw in prop::collection::vec((0u8..3, any::<usize>(), any::<usize>(), insert_strategy()), 2..3)) {
        let ops = realise(&base, &raw);
        let (x, y) = (&ops[0], &ops[1]);
        let yx = Edit { offset: y.0, delete: y.1 - y.0, insert: y.2.clone() };
        let xe = Edit { offset: x.0, delete: x.1 - x.0, insert: x.2.clone() };
        let (Ok(x2), Ok(y2)) = (
            transform_range(x.0..x.1, &yx, Priority::ThroughFirst),
            transform_range(y.0..y.1, &xe, Priority::SelfFirst),
        ) else {
            return Ok(());
        };
        let server = apply_str(&apply_str(&base, y.0..y.1, &y.2), x2.clone(), &x.2);
        let client = apply_str(&apply_str(&base, x.0..x.1, &x.2), y2, &y.2);
        prop_assert_eq!(&server, &client);

        let mut b = buf(&base);
        b.apply(req(vec![y.3.clone()]), &o("agent:y"), via(), 0).unwrap();
        let r = TxnRequest { cas: Cas::BaseRev(0), ..req(vec![x.3.clone()]) };
        b.apply(r, &o("agent:x"), via(), 0).unwrap();
        prop_assert_eq!(text(&b), server);
    }

    /// 6. Anchors vs a marker model: markers sit between chars and move with
    ///    the text; one inside a deleted span lands at its start.
    #[test]
    fn anchors_follow_marker_model(
        base in text_strategy(20),
        picks in prop::collection::vec((any::<usize>(), any::<bool>()), 1..5),
        raw in raw_ops(5),
    ) {
        let bs = boundaries(&base);
        let mut b = buf(&base);
        let mut model: Vec<(usize, Bias)> = Vec::new();
        for (i, (pick, after)) in picks.iter().enumerate() {
            let at = bs[pick % bs.len()];
            let bias = if *after { Bias::After } else { Bias::Before };
            b.anchor_set(&format!("m{i}"), AnchorSpec { at: Some(PosSpec::Offset(at)), bias: Some(bias), ..Default::default() }).unwrap();
            model.push((at, bias));
        }
        let ops = non_overlapping(realise(&base, &raw));
        let applied = b.apply(req(ops.iter().map(|o| o.3.clone()).collect()), &o("agent:a"), via(), 0).unwrap();
        for (i, (at, bias)) in model.iter().enumerate() {
            let predicted = marker_model(&base, *at, *bias, &applied.edits);
            prop_assert_eq!(b.anchor(&format!("m{i}")).unwrap().start.offset, predicted, "anchor m{} {:?}", i, bias);
        }
    }

    /// 7. Gap buffer vs `Vec<u8>`.
    #[test]
    fn gap_buffer_matches_vec(
        edits in prop::collection::vec((any::<usize>(), any::<usize>(), prop::collection::vec(any::<u8>(), 0..300)), 1..40)
    ) {
        let mut g = GapBuffer::new(false).unwrap();
        let mut v: Vec<u8> = Vec::new();
        for (x, y, bytes) in edits {
            let (p, q) = (x % (v.len() + 1), y % (v.len() + 1));
            let (s, e) = (p.min(q), p.max(q));
            g.replace(s..e, &bytes).unwrap();
            drop(v.splice(s..e, bytes));
            let mut out = Vec::new();
            g.extract_raw(0..g.len(), &mut out, 0);
            prop_assert_eq!(&out, &v);
            let mid = v.len() / 2;
            let fwd = g.read_forward(mid);
            prop_assert_eq!(fwd, &v[mid..mid + fwd.len()]);
        }
    }

    /// 8. An entry's inverse RangeSet, applied at its own rev, restores the
    ///    pre-entry text (multi-edit entries, same-offset inserts included).
    #[test]
    fn inverse_rangeset_restores(base in text_strategy(24), raw in raw_ops(6)) {
        let ops = non_overlapping(realise(&base, &raw));
        let mut b = buf(&base);
        b.apply(req(ops.iter().map(|o| o.3.clone()).collect()), &o("agent:a"), via(), 0).unwrap();
        let inv = invert(b.history(0, 1).next().unwrap());
        prop_assert_eq!(inv.rev, 1);
        let back: Vec<OpSpec> = inv.items.iter().map(|(r, t)| rep(r.start, r.end, t)).collect();
        b.apply(req(back), &o("agent:z"), via(), 0).unwrap();
        prop_assert_eq!(text(&b), base);
    }
}
