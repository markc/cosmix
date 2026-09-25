//! `view` tests (ced E1 plan, Stage E1a). The oracle is msedit's own
//! measurement over a contiguous `&[u8]` — the upstream path with no gap and
//! no adapter — so every gap-straddling result is checked against what
//! upstream computes for the same text.

use std::collections::HashMap;
use std::ops::Range;

use proptest::prelude::*;

use super::*;
use crate::ot::Edit;
use crate::text::Text;
use crate::vendor::msedit::document::ReadableDocument;
use crate::vendor::msedit::helpers::CoordType;
use crate::vendor::msedit::unicode::MeasurementConfig;

/// `s` as a `Text` whose gap sits at byte `p` (a char boundary).
fn with_gap(s: &str, p: usize) -> Text {
    let mut t = Text::from_text(s).unwrap();
    let pr = t.prepare(vec![Edit { offset: p, delete: 0, insert: "x".into() }]).unwrap();
    t.commit(pr);
    let pr = t.prepare(vec![Edit { offset: p, delete: 1, insert: String::new() }]).unwrap();
    t.commit(pr);
    if 0 < p && p < s.len() {
        assert_eq!(t.chunk_at(0).len(), p, "gap not placed at {p}");
    }
    let mut out = String::new();
    t.read(0..t.len(), &mut out);
    assert_eq!(out, s);
    t
}

fn char_boundaries(s: &str) -> Vec<usize> {
    (0..=s.len()).filter(|&p| s.is_char_boundary(p)).collect()
}

/// Upstream cluster boundaries of contiguous `s`, 0 and `len` included.
fn oracle_boundaries(s: &str) -> Vec<usize> {
    let bytes = s.as_bytes();
    let mut m = MeasurementConfig::new(&bytes);
    let mut out = vec![0];
    while *out.last().unwrap() < s.len() {
        let next = m.goto_offset(out.last().unwrap() + 1).offset;
        assert!(next > *out.last().unwrap(), "upstream did not advance");
        out.push(next);
    }
    out
}

/// Upstream `(boundary, cells)` for every boundary of contiguous `s`, cells
/// counted from the start of each boundary's line.
fn oracle_cells(s: &str, cfg: &MeasureCfg) -> HashMap<usize, usize> {
    let bytes = s.as_bytes();
    let mut out = HashMap::new();
    for b in oracle_boundaries(s) {
        let line_start = s[..b].rfind('\n').map_or(0, |i| i + 1);
        let at = MeasurementConfig::new(&bytes)
            .with_tab_size(CoordType::from(cfg.tab_size))
            .with_ambiguous_width(if cfg.ambiguous_wide { 2 } else { 1 })
            .with_cursor(cursor_at(line_start, 0))
            .goto_offset(b);
        out.insert(b, at.column as usize);
    }
    out
}

fn walk_next(t: &Text) -> Vec<usize> {
    let mut out = vec![0];
    while *out.last().unwrap() < t.len() {
        out.push(next_grapheme(t, *out.last().unwrap()));
    }
    out
}

fn walk_prev(t: &Text) -> Vec<usize> {
    let mut out = vec![t.len()];
    while *out.last().unwrap() > 0 {
        out.push(prev_grapheme(t, *out.last().unwrap()));
    }
    out.reverse();
    out
}

/// Every forward and backward chunk the adapter hands out ends on an
/// upstream boundary, is non-empty, and together they are the text.
fn assert_chunks(t: &Text, s: &str, bounds: &[usize]) {
    let doc = GraphemeDoc::new(t);
    let mut off = 0;
    let mut fwd = Vec::new();
    while off < s.len() {
        let c = doc.read_forward(off);
        assert!(!c.is_empty(), "empty chunk at {off}");
        fwd.extend_from_slice(c);
        off += c.len();
        assert!(bounds.contains(&off), "forward chunk ends inside a cluster at {off} in {s:?}");
    }
    assert_eq!(fwd, s.as_bytes());
    let mut off = s.len();
    let mut bwd: Vec<&[u8]> = Vec::new();
    while off > 0 {
        let c = doc.read_backward(off);
        assert!(!c.is_empty(), "empty backward chunk at {off}");
        off -= c.len();
        assert!(bounds.contains(&off), "backward chunk starts inside a cluster at {off} in {s:?}");
        bwd.push(c);
    }
    bwd.reverse();
    assert_eq!(bwd.concat(), s.as_bytes());
}

/// Boundaries, chunks and next/prev agree with upstream for the gap at
/// every char boundary of `s`.
fn assert_gap_independent(s: &str) {
    let bounds = oracle_boundaries(s);
    for p in char_boundaries(s) {
        let t = with_gap(s, p);
        assert_eq!(walk_next(&t), bounds, "next_grapheme walk, gap at {p}, {s:?}");
        assert_eq!(walk_prev(&t), bounds, "prev_grapheme walk, gap at {p}, {s:?}");
        assert_chunks(&t, s, &bounds);
        for o in 0..=s.len() {
            let next = bounds.iter().copied().find(|&b| b > o).unwrap_or(s.len());
            let floor = bounds.iter().copied().rfind(|&b| b <= o).unwrap();
            let prev = bounds.iter().copied().rfind(|&b| b < o).unwrap_or(0);
            assert_eq!(next_grapheme(&t, o), next, "next({o}), gap at {p}, {s:?}");
            assert_eq!(prev_grapheme(&t, o), prev, "prev({o}), gap at {p}, {s:?}");
            assert_eq!(GraphemeDoc::new(&t).floor(o), floor, "floor({o}), gap at {p}, {s:?}");
        }
    }
}

// ---------------------------------------------------------------- clusters at the gap

#[test]
fn combining_marks_straddle_the_gap() {
    let s = "xe\u{301}\u{302}y";
    assert_eq!(oracle_boundaries(s), [0, 1, 6, 7]);
    assert_gap_independent(s);
}

#[test]
fn zwj_emoji_straddles_the_gap() {
    let s = "a\u{1F469}\u{200D}\u{1F469}\u{200D}\u{1F467}b";
    assert_eq!(oracle_boundaries(s), [0, 1, 19, 20]);
    assert_gap_independent(s);
}

#[test]
fn regional_indicator_runs_pair_up_across_the_gap() {
    // Even: 🇦🇺🇳🇿 is two flags; odd: 🇦🇺🇳 is a flag and a lone indicator.
    let even = "a\u{1F1E6}\u{1F1FA}\u{1F1F3}\u{1F1FF}b";
    assert_eq!(oracle_boundaries(even), [0, 1, 9, 17, 18]);
    assert_gap_independent(even);
    let odd = "a\u{1F1E6}\u{1F1FA}\u{1F1F3}b";
    assert_eq!(oracle_boundaries(odd), [0, 1, 9, 13, 14]);
    assert_gap_independent(odd);
    // Parity is decided from the start of the run, not from the gap.
    let five = "\u{1F1E6}\u{1F1FA}\u{1F1F3}\u{1F1FF}\u{1F1E6}";
    assert_eq!(oracle_boundaries(five), [0, 8, 16, 20]);
    assert_gap_independent(five);
}

#[test]
fn crlf_is_one_cluster_across_the_gap() {
    let s = "a\r\nb";
    assert_eq!(oracle_boundaries(s), [0, 1, 3, 4]);
    assert_gap_independent(s);
    let t = with_gap(s, 2);
    assert_eq!(next_grapheme(&t, 1), 3);
    assert_eq!(prev_grapheme(&t, 3), 1);
}

#[test]
fn ten_kib_combining_cluster_is_never_split() {
    let marks = "\u{301}".repeat(5 * 1024); // 10 KiB of Extend
    let s = format!("a{marks}b");
    let end = 1 + marks.len();
    for p in [1, 1 + 2 * 2500, end - 2, end] {
        let t = with_gap(&s, p);
        assert_eq!(next_grapheme(&t, 0), end, "gap at {p}");
        assert_eq!(next_grapheme(&t, 4000), end, "gap at {p}");
        assert_eq!(prev_grapheme(&t, end), 0, "gap at {p}");
        assert_eq!(prev_grapheme(&t, 4001), 0, "gap at {p}");
        let doc = GraphemeDoc::new(&t);
        if p > 1 && p < end {
            assert_eq!(doc.read_forward(0).len(), end, "stitched whole, gap at {p}");
            assert_eq!(doc.read_backward(end).len(), end, "stitched whole, gap at {p}");
        }
        let cfg = MeasureCfg::default();
        let cs: Vec<_> = clusters(&t, &cfg, 0..s.len(), 0).collect();
        assert_eq!(cs.len(), 2, "gap at {p}");
        assert_eq!(cs[0].range, 0..end);
        assert_eq!(cs[0].cells, 1);
        assert!(!cs[0].ascii);
        assert_eq!(cs[1], Cluster { range: end..end + 1, cells: 1, is_tab: false, ascii: true });
    }
    assert_chunks(&with_gap(&s, 1 + 2 * 2500), &s, &oracle_boundaries(&s));
}

#[test]
fn next_and_prev_round_trip() {
    let s = "ab\te\u{301}漢\u{1F1E6}\u{1F1FA}\r\n\u{1F469}\u{200D}\u{1F467}x\n\ny";
    for p in char_boundaries(s) {
        let t = with_gap(s, p);
        let bounds = oracle_boundaries(s);
        for w in bounds.windows(2) {
            assert_eq!(next_grapheme(&t, w[0]), w[1]);
            assert_eq!(prev_grapheme(&t, w[1]), w[0]);
            assert_eq!(prev_grapheme(&t, next_grapheme(&t, w[0])), w[0]);
            assert_eq!(next_grapheme(&t, prev_grapheme(&t, w[1])), w[1]);
        }
        assert_eq!(next_grapheme(&t, s.len()), s.len());
        assert_eq!(next_grapheme(&t, s.len() + 7), s.len());
        assert_eq!(prev_grapheme(&t, 0), 0);
    }
}

#[test]
fn certain_boundaries_are_boundaries() {
    let s = "ab\te\u{301}漢\u{1F1E6}\u{1F1FA}\u{1F1F3}\r\n\u{1F469}\u{200D}\u{1F467}x\r\u{915}\u{94D}\u{937}\n";
    let bounds = oracle_boundaries(s);
    let t = with_gap(s, 0);
    let doc = GraphemeDoc::new(&t);
    for p in 0..=s.len() {
        if doc.certain(p) {
            assert!(bounds.contains(&p), "certain({p}) is not a boundary");
        }
    }
    // After `\n`, around CR/LF/Control, and between two ASCII letters.
    for p in [1, 2, 3, s.find('\r').unwrap(), s.find('\n').unwrap() + 1] {
        assert!(doc.certain(p), "{p} should be certain");
    }
}

// ---------------------------------------------------------------- measurement

#[test]
fn ambiguous_width_is_per_call() {
    let t = Text::from_text("α|").unwrap();
    let narrow = MeasureCfg { tab_size: 4, ambiguous_wide: false };
    let wide = MeasureCfg { tab_size: 4, ambiguous_wide: true };
    assert_eq!(visual_of(&t, &narrow, 2), VisualPos { line: 1, cells: 1 });
    assert_eq!(visual_of(&t, &wide, 2), VisualPos { line: 1, cells: 2 });
    assert_eq!(offset_at(&t, &wide, 1, 1, Round::Left), 0);
    assert_eq!(offset_at(&t, &wide, 1, 1, Round::Right), 2);
    assert_eq!(offset_at(&t, &narrow, 1, 1, Round::Left), 2);
    // Interleaved calls do not leak into each other.
    assert_eq!(visual_of(&t, &narrow, 3).cells, 2);
    assert_eq!(visual_of(&t, &wide, 3).cells, 3);
}

#[test]
fn tabs_expand_to_the_next_stop() {
    let t = Text::from_text("a\tb\nabcd\tx\n\t\t").unwrap();
    let four = MeasureCfg { tab_size: 4, ambiguous_wide: false };
    let eight = MeasureCfg { tab_size: 8, ambiguous_wide: false };
    assert_eq!(visual_of(&t, &four, 2), VisualPos { line: 1, cells: 4 });
    assert_eq!(visual_of(&t, &four, 3), VisualPos { line: 1, cells: 5 });
    assert_eq!(visual_of(&t, &eight, 2).cells, 8);
    assert_eq!(visual_of(&t, &four, 4 + 5).cells, 8);
    assert_eq!(visual_of(&t, &four, 11 + 2).cells, 8);
    // Inside a tab: Left before it, Right after it, Nearest by distance.
    assert_eq!(offset_at(&t, &four, 1, 2, Round::Left), 1);
    assert_eq!(offset_at(&t, &four, 1, 2, Round::Right), 2);
    assert_eq!(offset_at(&t, &four, 1, 2, Round::Nearest), 1);
    assert_eq!(offset_at(&t, &four, 1, 3, Round::Nearest), 2);
    // Clamped to the content end.
    assert_eq!(offset_at(&t, &four, 1, 99, Round::Left), 3);
    assert_eq!(offset_at(&t, &four, 1, usize::MAX, Round::Nearest), 3);
    // Out-of-range lines clamp.
    assert_eq!(offset_at(&t, &four, 0, 0, Round::Left), 0);
    assert_eq!(offset_at(&t, &four, 99, 4, Round::Left), 12);
    // A tab mid-run expands from the given start column; clamped tab sizes.
    let cs: Vec<_> = clusters(&t, &four, 1..3, 2).collect();
    assert_eq!(cs[0], Cluster { range: 1..2, cells: 2, is_tab: true, ascii: true });
    let zero = MeasureCfg { tab_size: 0, ambiguous_wide: false };
    assert_eq!(visual_of(&t, &zero, 2).cells, 2);
    let huge = MeasureCfg { tab_size: 200, ambiguous_wide: false };
    assert_eq!(visual_of(&t, &huge, 2).cells, 16);
}

#[test]
fn wide_and_crlf_lines() {
    let t = Text::from_text("漢a\r\nb\r").unwrap();
    let cfg = MeasureCfg::default();
    assert_eq!(visual_of(&t, &cfg, 3), VisualPos { line: 1, cells: 2 });
    assert_eq!(offset_at(&t, &cfg, 1, 1, Round::Left), 0);
    assert_eq!(offset_at(&t, &cfg, 1, 1, Round::Right), 3);
    assert_eq!(offset_at(&t, &cfg, 1, 1, Round::Nearest), 0);
    // The CRLF is not content: the line ends before the `\r`, and offsets
    // inside the CRLF measure as its start.
    assert_eq!(offset_at(&t, &cfg, 1, 99, Round::Left), 4);
    assert_eq!(visual_of(&t, &cfg, 5), VisualPos { line: 1, cells: 3 });
    assert_eq!(clusters(&t, &cfg, 0..6, 0).count(), 2);
    // A trailing lone `\r` is content.
    assert_eq!(offset_at(&t, &cfg, 2, 99, Round::Left), t.len());
    assert_eq!(visual_of(&t, &cfg, 6), VisualPos { line: 2, cells: 0 });
}

#[test]
fn words() {
    let t = with_gap("Hello World, e\u{301}t\u{301}e\u{301}\n\nend", 17);
    assert_eq!(word_next(&t, 0), 5);
    assert_eq!(word_next(&t, 5), 11);
    assert_eq!(word_prev(&t, 11), 6);
    assert_eq!(word_at(&t, 7), 6..11);
    assert_eq!(word_at(&t, 3), 0..5);
    // The accented word straddles the gap and is selected whole.
    assert_eq!(word_at(&t, 15), 13..22);
    assert_eq!(word_next(&t, 13), 22);
    assert_eq!(word_prev(&t, 22), 13);
    // Upstream's own cases through the adapter.
    for (s, from, want) in [("Hello,World", 0, 5), ("   Hello", 0, 8), ("\n\nHello", 0, 1)] {
        assert_eq!(word_next(&Text::from_text(s).unwrap(), from), want, "{s:?}");
    }
    for (s, from, want) in [("Hello,World", 10, 6), ("Hello   ", 7, 0), ("Hello\n\n", 7, 6)] {
        assert_eq!(word_prev(&Text::from_text(s).unwrap(), from), want, "{s:?}");
    }
}

#[test]
fn word_results_are_cluster_boundaries() {
    // A space followed by a combining mark is one cluster; word motion must
    // not stop between them.
    let s = "ab \u{301}cd ef";
    let bounds = oracle_boundaries(s);
    for p in char_boundaries(s) {
        let t = with_gap(s, p);
        for o in 0..=s.len() {
            assert!(bounds.contains(&word_next(&t, o)), "word_next({o}), gap at {p}");
            assert!(bounds.contains(&word_prev(&t, o)), "word_prev({o}), gap at {p}");
            let r = word_at(&t, o);
            assert!(bounds.contains(&r.start) && bounds.contains(&r.end), "word_at({o}) = {r:?}");
        }
    }
}

// ---------------------------------------------------------------- properties

fn alphabet() -> impl Strategy<Value = char> {
    prop::sample::select(vec![
        'a', 'b', ' ', '\t', '\n', '\r', ',', 'é', 'e', '\u{301}', '\u{200D}', '\u{1F469}',
        '\u{1F3FB}', '\u{1F1E6}', '\u{1F1FA}', '漢', 'α', '\u{1100}', '\u{1161}', '\u{11A8}',
        '\u{915}', '\u{94D}', '\u{600}', '\u{FE0F}', '\u{7}',
    ])
}

fn arb_text() -> impl Strategy<Value = String> {
    prop::collection::vec(alphabet(), 0..32).prop_map(|v| v.into_iter().collect())
}

/// For each line of `s`: `(content range, [(boundary, cells)])` in order.
fn oracle_lines(s: &str, cfg: &MeasureCfg) -> Vec<(Range<usize>, Vec<(usize, usize)>)> {
    let cells = oracle_cells(s, cfg);
    let mut bounds: Vec<_> = cells.keys().copied().collect();
    bounds.sort_unstable();
    let mut out = Vec::new();
    let mut start = 0;
    for piece in s.split('\n') {
        let mut end = start + piece.len();
        if end < s.len() && piece.ends_with('\r') {
            end -= 1;
        }
        let pts = bounds.iter().filter(|&&b| b >= start && b <= end).map(|&b| (b, cells[&b])).collect();
        out.push((start..end, pts));
        start += piece.len() + 1;
    }
    out
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn boundaries_and_chunks_are_gap_independent(s in arb_text()) {
        assert_gap_independent(&s);
    }

    #[test]
    fn visual_of_and_offset_at_are_inverses(s in arb_text(), gap in any::<prop::sample::Index>(),
                                            tab in 1u8..=8, wide in any::<bool>()) {
        let cfg = MeasureCfg { tab_size: tab, ambiguous_wide: wide };
        let cb = char_boundaries(&s);
        let t = with_gap(&s, cb[gap.index(cb.len())]);
        for (i, (content, pts)) in oracle_lines(&s, &cfg).into_iter().enumerate() {
            let line = i + 1;
            // Every boundary measures as upstream; clusters() agrees.
            for &(b, c) in &pts {
                prop_assert_eq!(visual_of(&t, &cfg, b), VisualPos { line, cells: c });
            }
            let walked: Vec<_> = clusters(&t, &cfg, content.clone(), 0)
                .scan(0, |acc, cl| { *acc += usize::from(cl.cells); Some((cl.range.end, *acc)) })
                .collect();
            prop_assert_eq!(&walked[..], &pts[1..]);
            // offset_at inverts visual_of: Left is the first boundary at the
            // cell (else the last before it), Right the first at or after.
            let max = pts.last().unwrap().1;
            for c in 0..=max + 2 {
                let left = pts.iter().find(|p| p.1 == c)
                    .or_else(|| pts.iter().rev().find(|p| p.1 < c)).unwrap();
                let right = pts.iter().find(|p| p.1 >= c).unwrap_or(left);
                let nearest = if right.1 - c.min(right.1) < c - left.1 { right } else { left };
                prop_assert_eq!(offset_at(&t, &cfg, line, c, Round::Left), left.0, "L c={}", c);
                prop_assert_eq!(offset_at(&t, &cfg, line, c, Round::Right), right.0, "R c={}", c);
                prop_assert_eq!(offset_at(&t, &cfg, line, c, Round::Nearest), nearest.0, "N c={}", c);
                let back = visual_of(&t, &cfg, offset_at(&t, &cfg, line, c, Round::Left));
                prop_assert!(back.line == line && back.cells <= c);
            }
            // Offsets inside clusters measure as the cluster start.
            for o in content.clone() {
                let floor = pts.iter().rev().find(|p| p.0 <= o).unwrap();
                prop_assert_eq!(visual_of(&t, &cfg, o).cells, floor.1);
            }
        }
    }
}

// ---------------------------------------------------------------- long lines

/// A ~`bytes`-long single line of mixed content, then `"\nend"`.
fn long_line(bytes: usize) -> String {
    let unit = "abc\tdef 漢字e\u{301} \u{1F1E6}\u{1F1FA}x\u{1F469}\u{200D}\u{1F467}yz, α\t";
    let mut s = unit.repeat(bytes / unit.len() + 1);
    s.push_str("\nend");
    s
}

fn mid_char_boundary(s: &str) -> usize {
    (s.len() / 2..s.len()).find(|&p| s.is_char_boundary(p)).unwrap()
}

#[test]
fn checkpoints_equal_a_full_walk() {
    let s = long_line(64 * 1024);
    let t = with_gap(&s, mid_char_boundary(&s));
    let cfg = MeasureCfg::default();
    let content = t.line_range(1).unwrap();
    let mut cells_at = HashMap::from([(0usize, 0usize)]);
    let mut ends = vec![0usize];
    let mut acc = 0;
    for cl in clusters(&t, &cfg, content.clone(), 0) {
        acc += usize::from(cl.cells);
        cells_at.insert(cl.range.end, acc);
        ends.push(cl.range.end);
    }
    let cps = line_checkpoints(&t, &cfg, 1);
    assert_eq!(cps[0], (0, 0));
    assert_eq!(cps.len(), content.end.div_ceil(CHECKPOINT_EVERY));
    for (k, &(o, c)) in cps.iter().enumerate() {
        assert_eq!(cells_at.get(&o), Some(&c), "checkpoint {k} at {o}");
        let first = *ends.iter().find(|&&e| e >= k * CHECKPOINT_EVERY).unwrap();
        assert_eq!(o, first, "checkpoint {k} is the first boundary at or after {}", k * CHECKPOINT_EVERY);
        assert_eq!(visual_of(&t, &cfg, o).cells, c);
    }
    // Seeking with checkpoints agrees with seeking without.
    for c in (0..acc).step_by(997) {
        for round in [Round::Left, Round::Right, Round::Nearest] {
            assert_eq!(
                offset_at_with(&t, &cfg, 1, c, round, &cps),
                offset_at(&t, &cfg, 1, c, round),
                "cells {c} {round:?}"
            );
        }
    }
    for &o in ends.iter().step_by(313) {
        assert_eq!(visual_of_with(&t, &cfg, o, &cps), visual_of(&t, &cfg, o));
    }
    // Another line's checkpoints are ignored.
    assert_eq!(visual_of_with(&t, &cfg, t.len(), &cps), VisualPos { line: 2, cells: 3 });
    assert_eq!(offset_at_with(&t, &cfg, 2, 1, Round::Left, &cps), content.end + 2);
}

fn touched<T>(f: impl FnOnce() -> T) -> (T, usize) {
    TOUCHED.with(|t| t.set(0));
    let out = f();
    (out, TOUCHED.with(|t| t.get()))
}

#[test]
fn five_mib_line_seeks_touch_at_most_8_kib() {
    let s = long_line(5 * 1024 * 1024);
    let t = with_gap(&s, mid_char_boundary(&s));
    let cfg = MeasureCfg::default();
    let end = t.line_range(1).unwrap().end;
    assert!(end >= 5 * 1024 * 1024);

    // End: the content end, without reading the line.
    let (o, n) = touched(|| offset_at(&t, &cfg, 1, usize::MAX, Round::Left));
    assert_eq!(o, end);
    assert!(n <= 8 * 1024, "End touched {n} bytes");

    // One full walk for the truth, and the checkpoints the widget caches.
    let total = visual_of(&t, &cfg, end).cells;
    let cps = line_checkpoints(&t, &cfg, 1);
    let last = *cps.last().unwrap();
    let tail: Vec<_> = clusters(&t, &cfg, last.0..end, last.1).collect();
    let near_end = total - 3;
    let want = {
        let mut acc = last.1;
        let mut at = last.0;
        for cl in &tail {
            if acc == near_end || acc + usize::from(cl.cells) > near_end {
                break;
            }
            acc += usize::from(cl.cells);
            at = cl.range.end;
        }
        at
    };

    // A seek to the end's last cells, and the caret at the end, from the
    // cached checkpoints.
    let (o, n) = touched(|| offset_at_with(&t, &cfg, 1, near_end, Round::Left, &cps));
    assert_eq!(o, want);
    assert!(n <= 8 * 1024, "offset_at_with touched {n} bytes");
    let (v, n) = touched(|| visual_of_with(&t, &cfg, end, &cps));
    assert_eq!(v, VisualPos { line: 1, cells: total });
    assert!(n <= 8 * 1024, "visual_of_with touched {n} bytes");

    // And the same answer without checkpoints (a full walk).
    assert_eq!(offset_at(&t, &cfg, 1, near_end, Round::Left), want);
}
