//! Stage E1b tests (ced E1 plan §6): multi-chunk sources, random seeks equal a
//! linear parse (also after an edit + `invalidate_from`), time-sliced
//! advancing, and `MAX_LINE_LEN`.

use cosmix_lsh::LineSource;
use cosmix_lsh::cache::{Cache, INTERVAL};
use cosmix_lsh::highlighter::{Highlighter, MAX_LINE_LEN, Span};
use cosmix_lsh::runtime::Language;
use proptest::prelude::*;

fn rust() -> &'static Language {
    cosmix_lsh::language_by_id("rust").expect("lsh defines rust")
}

/// A source whose chunks end at every multiple of `size` — a line may span
/// many of them, as a gap buffer's does around the gap.
struct Chunked<'a> {
    data: &'a [u8],
    size: usize,
}

impl LineSource for Chunked<'_> {
    fn read_forward(&self, offset: usize) -> &[u8] {
        if offset >= self.data.len() {
            return &[];
        }
        let end = ((offset / self.size) + 1) * self.size;
        &self.data[offset..end.min(self.data.len())]
    }
}

/// Every line's spans, parsed top to bottom by one highlighter.
fn linear(src: &dyn LineSource, lines: usize) -> Vec<Vec<Span>> {
    let mut h = Highlighter::new(src, rust());
    (0..lines)
        .map(|_| {
            let mut out = Vec::new();
            h.parse_next_line(&mut out);
            out
        })
        .collect()
}

const POOL: &[&str] = &[
    "fn main() {",
    "    let x = \"a string // not a comment\";",
    "    // a line comment with \"quotes\"",
    "    /* a block comment opens",
    "       and continues here",
    "    */ let after = 42;",
    "}",
    "",
    "#[derive(Debug)]",
    "pub struct S<'a> { r: &'a str, n: u64 }",
    "    let s = r#\"raw \" string\"#;",
    "    let c = 'c'; let e = '\\n';",
    "impl S<'_> { pub fn n(&self) -> u64 { self.n * 0x10 + 1_000 } }",
    "    \"unterminated string continues",
    "    onto this line\";",
    "\t// tab-indented comment\r",
];

fn build(picks: &[usize]) -> String {
    let mut text = String::new();
    for &p in picks {
        text.push_str(POOL[p % POOL.len()]);
        text.push('\n');
    }
    text
}

#[test]
fn spans_are_absolute_and_classified() {
    let text = "fn main() {\n    // hello\n}\n";
    let src = text.as_bytes();
    let spans = linear(&src, 3);
    let comment = spans[1].iter().find(|s| format!("{:?}", s.kind).contains("Comment")).expect("line 2 has a comment span");
    assert_eq!(comment.start, text.find("//").unwrap());
    for line in &spans {
        assert!(line.windows(2).all(|w| w[0].start < w[1].start), "spans are ordered: {line:?}");
    }
    assert!(spans[0].iter().all(|s| s.start < text.find('\n').unwrap()));
}

#[test]
fn multi_chunk_sources_equal_one_slice() {
    let picks: Vec<usize> = (0..400).map(|i| (i * 7 + i / 3) % POOL.len()).collect();
    let text = build(&picks);
    let flat = text.as_bytes();
    let want = linear(&flat, picks.len() + 1);
    for size in [1, 2, 3, 7, 64, 4096] {
        let src = Chunked { data: text.as_bytes(), size };
        assert_eq!(linear(&src, picks.len() + 1), want, "chunk size {size}");
    }
}

#[test]
fn interval_is_pinned() {
    assert_eq!(INTERVAL, 1024);
}

#[test]
fn over_long_lines_are_plain_and_keep_line_numbers() {
    let long = "x".repeat(MAX_LINE_LEN + 8 * 1024);
    let just_under = format!("// {}", "y".repeat(MAX_LINE_LEN - 5));
    let text = format!("// before\n{long}\n// after\n{just_under}\nfn f() {{}}\n");
    let tail = text.find("// after").unwrap();
    let last = text.find("fn f()").unwrap();
    for size in [7, 1000, 1 << 20] {
        let src = Chunked { data: text.as_bytes(), size };
        let mut h = Highlighter::new(&src, rust());
        let mut out = Vec::new();
        h.parse_next_line(&mut out);
        assert!(!out.is_empty());
        h.parse_next_line(&mut out);
        assert!(out.is_empty(), "a {}-byte line is not highlighted", long.len());
        assert_eq!(h.line(), 3);
        h.parse_next_line(&mut out);
        assert_eq!(out.first().map(|s| s.start), Some(tail), "the line after it starts where it should (chunk {size})");
        h.parse_next_line(&mut out);
        assert!(!out.is_empty(), "a line one byte short of the cap (newline included) is highlighted");
        h.parse_next_line(&mut out);
        assert_eq!(out.first().map(|s| s.start), Some(last));
        assert_eq!(h.line(), 6);
    }
}

#[test]
fn past_the_end_is_empty() {
    let text = "fn a() {}";
    let src = text.as_bytes();
    let mut h = Highlighter::new(&src, rust());
    let mut out = Vec::new();
    h.parse_next_line(&mut out);
    assert!(!out.is_empty(), "a last line without a newline is highlighted");
    h.parse_next_line(&mut out);
    assert!(out.is_empty());
}

#[test]
fn advance_makes_progress_below_one_interval_per_call() {
    let picks: Vec<usize> = (0..3 * INTERVAL + 10).map(|i| i % POOL.len()).collect();
    let text = build(&picks);
    let src = text.as_bytes();
    let want = linear(&src, picks.len());
    let target = picks.len() - 3;

    let mut cache = Cache::new();
    let mut calls = 0;
    loop {
        // A fresh highlighter per call, as a per-frame caller has.
        let mut h = Highlighter::new(&src, rust());
        calls += 1;
        if cache.advance(&mut h, target, 100) {
            break;
        }
        assert!(calls < 100, "no progress");
    }
    assert!(calls >= 3 * INTERVAL / 100, "each call parsed at most 100 lines");
    assert!(cache.reach() > (target - 1) / INTERVAL * INTERVAL);

    let mut h = Highlighter::new(&src, rust());
    let mut out = Vec::new();
    cache.parse_line(&mut h, target, &mut out);
    assert_eq!(out, want[target - 1]);
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 24, ..ProptestConfig::default() })]

    #[test]
    fn random_seeks_equal_a_linear_parse(
        picks in proptest::collection::vec(0..POOL.len(), 2 * INTERVAL..3 * INTERVAL),
        seeks in proptest::collection::vec((any::<proptest::sample::Index>(), any::<bool>()), 1..40),
        edit in (any::<proptest::sample::Index>(), 0..POOL.len()),
        chunk in prop_oneof![Just(5usize), Just(333), Just(1 << 20)],
    ) {
        let lines = picks.len() + 1;
        let text = build(&picks);
        let src = Chunked { data: text.as_bytes(), size: chunk };
        let want = linear(&src, lines);

        let mut cache = Cache::new();
        let mut kept = Highlighter::new(&src, rust());
        let mut out = Vec::new();
        for (at, fresh) in &seeks {
            let line = at.index(lines) + 1;
            if *fresh {
                let mut h = Highlighter::new(&src, rust());
                cache.parse_line(&mut h, line, &mut out);
            } else {
                cache.parse_line(&mut kept, line, &mut out);
            }
            prop_assert_eq!(&out, &want[line - 1], "line {}", line);
        }

        // Edit one line, invalidate from it, and seek again.
        let mut edited = picks.clone();
        let (at, with) = edit;
        let changed = at.index(edited.len());
        edited[changed] = with;
        let text2 = build(&edited);
        let src2 = Chunked { data: text2.as_bytes(), size: chunk };
        let want2 = linear(&src2, lines);
        cache.invalidate_from(changed + 1);
        for (at, _) in &seeks {
            let line = at.index(lines) + 1;
            let mut h = Highlighter::new(&src2, rust());
            cache.parse_line(&mut h, line, &mut out);
            prop_assert_eq!(&out, &want2[line - 1], "after the edit, line {}", line);
        }
    }
}
