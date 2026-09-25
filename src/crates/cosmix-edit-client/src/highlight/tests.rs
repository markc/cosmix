//! Highlight (plan §7.1): lsh spans, Mix spans, invalidation from the touched
//! line, stale Mix results, cold-seek time slicing.

use std::path::Path;

use cosmix_edit_core::ot::Edit;
use cosmix_edit_core::text::Text;

use super::*;
use crate::types::{DeltaKind, ViewDelta};

fn classes(h: &mut Highlight, text: &Text, s: &str, line: usize) -> Vec<(String, HlClass)> {
    let mut budget = SliceBudget { max_lines: usize::MAX };
    h.spans(text, line, &mut budget).expect("reached").iter().map(|(r, c)| (s[r.clone()].to_string(), *c)).collect()
}

fn class_of(h: &mut Highlight, text: &Text, s: &str, line: usize, needle: &str) -> Option<HlClass> {
    classes(h, text, s, line).into_iter().find(|(t, _)| t.contains(needle)).map(|(_, c)| c)
}

/// Apply `edits` (sequential) to `text` and `s`, returning the delta.
fn edit(text: &mut Text, s: &mut String, edits: Vec<Edit>, view_gen: u64) -> ViewDelta {
    for e in &edits {
        s.replace_range(e.offset..e.offset + e.delete, &e.insert);
    }
    let p = text.prepare(edits.clone()).unwrap();
    text.commit(p);
    ViewDelta { edits, origin: None, kind: DeltaKind::Remote, rev: view_gen, view_gen }
}

#[test]
fn lsh_spans_for_rust_and_markdown() {
    let s = "fn main() {\n    // hello\n    let x = \"str\";\n}\n";
    let text = Text::from_text(s).unwrap();
    let mut h = Highlight::for_language("rust", None);
    assert_eq!(class_of(&mut h, &text, s, 1, "fn"), Some(HlClass::Keyword));
    assert_eq!(class_of(&mut h, &text, s, 2, "hello"), Some(HlClass::Comment));
    assert_eq!(class_of(&mut h, &text, s, 3, "str"), Some(HlClass::String));
    let md = "# Title\n\nsome *text*\n";
    let text = Text::from_text(md).unwrap();
    let mut h = Highlight::for_language("markdown", None);
    assert_eq!(class_of(&mut h, &text, md, 1, "Title"), Some(HlClass::Heading));
    for (_, c) in classes(&mut h, &text, md, 3) {
        assert_ne!(c, HlClass::Plain, "plain runs are not emitted as spans");
    }
}

#[test]
fn language_selection() {
    for id in ["c", "cpp", "diff", "git_commit", "go", "javascript", "json", "lua", "markdown", "python", "rust", "shell", "toml", "xml", "yaml"] {
        assert!(lsh_language(id).is_some(), "editd language {id} has an lsh definition");
    }
    assert!(lsh_language("text").is_none());
    let h = Highlight::for_language("text", Some(Path::new("/x/Program.cs")));
    assert!(format!("{h:?}").contains("lsh:csharp"), "{h:?}: path associations are the fallback");
    let h = Highlight::for_language("text", Some(Path::new("/x/notes.txt")));
    assert!(format!("{h:?}").contains("plain"));
    assert!(Highlight::for_language("scene", None).is_mix());
    assert_eq!(Highlight::for_language("mix-data", None).language(), "mix-data");
}

#[test]
fn lsh_invalidates_from_the_touched_line() {
    let mut s: String = (0..3000).map(|i| format!("let x{i} = {i};\n")).collect();
    let mut text = Text::from_text(&s).unwrap();
    let mut h = Highlight::for_language("rust", None);
    assert_eq!(class_of(&mut h, &text, &s, 2999, "let"), Some(HlClass::Keyword));
    // Open a block comment on line 5: every later line becomes a comment.
    let at = text.line_start(5).unwrap();
    let d = edit(&mut text, &mut s, vec![Edit { offset: at, delete: 0, insert: "/*".into() }], 1);
    h.apply_delta(&text, &d);
    assert_eq!(class_of(&mut h, &text, &s, 2999, "let"), Some(HlClass::Comment));
    assert_eq!(class_of(&mut h, &text, &s, 4, "let"), Some(HlClass::Keyword), "lines before the edit keep their spans");
    let at = text.line_start(5).unwrap();
    let d = edit(&mut text, &mut s, vec![Edit { offset: at, delete: 2, insert: String::new() }], 2);
    h.apply_delta(&text, &d);
    assert_eq!(class_of(&mut h, &text, &s, 2999, "let"), Some(HlClass::Keyword));
    // Scrolling back one line re-parses at most FINE_INTERVAL lines.
    let mut budget = SliceBudget { max_lines: FINE_INTERVAL };
    assert!(h.spans(&text, 2900, &mut budget).is_some());
}

#[test]
fn cold_seek_is_time_sliced() {
    let lines = 2_000_000;
    let s = "x = 1\n".repeat(lines);
    let text = Text::from_text(&s).unwrap();
    let mut h = Highlight::for_language("python", None);
    let mut frames = 0;
    loop {
        let mut budget = SliceBudget::default();
        let started = std::time::Instant::now();
        let got = h.spans(&text, lines, &mut budget).is_some();
        let spent = started.elapsed();
        assert!(spent < std::time::Duration::from_millis(50), "frame {frames} took {spent:?}");
        frames += 1;
        if got {
            break;
        }
        assert!(h.behind());
        assert_eq!(budget.max_lines, 0, "a behind frame used its whole budget, no more");
        assert!(frames < lines, "no progress");
    }
    assert!(!h.behind());
    assert!(frames >= lines / SliceBudget::default().max_lines - 1, "{frames} frames: the seek was sliced");
    // The next frame near the end is cheap.
    let mut budget = SliceBudget { max_lines: 10 };
    assert!(h.spans(&text, lines - 1, &mut budget).is_some() || h.spans(&text, lines, &mut budget).is_some());
}

#[cfg(feature = "mix")]
fn tag(view_gen: u64) -> ResultTag {
    ResultTag { epoch: "e1".into(), buffer: "b1".into(), view_gen, language: "scene".into(), cfg: 0 }
}

#[cfg(feature = "mix")]
#[test]
fn mix_spans_for_a_scene() {
    let s = "-- panel\nwindow {\n  title: \"Hi\"\n  width: 42\n  on: true\n}\n";
    let text = Text::from_text(s).unwrap();
    let mut h = Highlight::for_language("scene", Some(Path::new("/x/scene.mix")));
    let mut budget = SliceBudget::default();
    assert_eq!(h.spans(&text, 1, &mut budget), Some(EMPTY), "plain until the first result");
    let (t, src) = h.mix_request(&text, tag(0)).expect("due");
    assert!(h.mix_request(&text, tag(0)).is_none(), "one request per gen");
    h.mix_result(t, run_mix("scene", &src));
    assert_eq!(class_of(&mut h, &text, s, 1, "panel"), Some(HlClass::Comment));
    assert_eq!(class_of(&mut h, &text, s, 3, "Hi"), Some(HlClass::String));
    assert_eq!(class_of(&mut h, &text, s, 4, "42"), Some(HlClass::Number));
    assert_eq!(class_of(&mut h, &text, s, 5, "true"), Some(HlClass::Constant));
    assert!(h.mix_request(&text, tag(0)).is_none(), "fresh: nothing due");
}

#[cfg(feature = "mix")]
#[test]
fn stale_mix_results_cover_only_lines_before_the_touch() {
    let mut s = "-- a\n-- b\nx = 1\n-- d\n-- e\n".to_string();
    let mut text = Text::from_text(&s).unwrap();
    let mut h = Highlight::for_language("scene", None);
    let (t0, src0) = h.mix_request(&text, tag(0)).unwrap();
    let old = run_mix("scene", &src0);
    // A quote typed on line 3 before the gen-0 result lands.
    let at = text.line_start(3).unwrap();
    let d = edit(&mut text, &mut s, vec![Edit { offset: at, delete: 0, insert: "\"".into() }], 1);
    h.apply_delta(&text, &d);
    h.mix_result(t0, old);
    assert_eq!(class_of(&mut h, &text, &s, 1, "a"), Some(HlClass::Comment));
    assert_eq!(class_of(&mut h, &text, &s, 2, "b"), Some(HlClass::Comment));
    for line in 3..=5 {
        assert!(classes(&mut h, &text, &s, line).is_empty(), "line {line} stays plain until the fresh result");
    }
    let (t1, src1) = h.mix_request(&text, tag(1)).unwrap();
    h.mix_result(t1, run_mix("scene", &src1));
    assert!(classes(&mut h, &text, &s, 3).iter().any(|(_, c)| *c == HlClass::Invalid), "the fresh result sees the open quote");

    // A different epoch or language discards a result.
    let mut other = tag(1);
    other.epoch = "e2".into();
    let before = classes(&mut h, &text, &s, 1);
    h.mix_result(other, vec![(0..4, cosmix_mix::lexer::TokenClass::Keyword)]);
    assert_eq!(classes(&mut h, &text, &s, 1), before);
    let mut lang = tag(1);
    lang.language = "mix".into();
    h.mix_result(lang, vec![(0..4, cosmix_mix::lexer::TokenClass::Keyword)]);
    assert_eq!(classes(&mut h, &text, &s, 1), before);
    // An older result never replaces a newer one.
    h.mix_result(tag(0), vec![(0..4, cosmix_mix::lexer::TokenClass::Keyword)]);
    assert_eq!(classes(&mut h, &text, &s, 1), before);
}

#[cfg(feature = "mix")]
#[test]
fn large_mix_buffers_stay_plain() {
    let s = "x = 1\n".repeat(MIX_MAX_BYTES / 6 + 1);
    let text = Text::from_text(&s).unwrap();
    let mut h = Highlight::for_language("mix", None);
    let mut t = tag(0);
    t.language = "mix".into();
    assert!(h.mix_request(&text, t).is_none());
}
