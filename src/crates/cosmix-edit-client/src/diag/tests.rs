//! Diagnostics (plan §4.10): schema check, tag check, positions, covered-line
//! invalidation vs mapping.

use cosmix_edit_core::ot::Edit;

use super::*;
use crate::highlight::ResultTag;
use crate::types::{DeltaKind, ViewDelta};

fn tag(view_gen: u64) -> ResultTag {
    ResultTag { epoch: "e".into(), buffer: "b".into(), view_gen, language: "mix".into(), cfg: 9 }
}

fn report(diags: &str) -> String {
    format!(r#"{{"schema_version":2,"tool":"mix lint","diagnostics":[{diags}]}}"#)
}

fn delta(edits: Vec<Edit>) -> ViewDelta {
    ViewDelta { edits, origin: None, kind: DeltaKind::Remote, rev: 1, view_gen: 1 }
}

const TEXT: &str = "a = 1\n  $bad_name = 2\nc = 3\n";

#[test]
fn positions_come_from_line_and_column() {
    let mut d = Diagnostics::default();
    let json = report(
        r#"{"code":"E1","severity":"error","file":"-","line":2,"column":3,"message":"m","hint":"h"},
           {"code":"W1","severity":"warning","file":"-","line":3,"column":null,"message":"w","hint":null},
           {"code":"N1","severity":"note","file":"-","line":null,"column":null,"message":"n","hint":null}"#,
    );
    d.accept(&tag(0), tag(0), TEXT, &json, &[]).unwrap();
    let items = d.items();
    assert_eq!(items.len(), 3);
    assert_eq!(&TEXT[items[0].range.clone()], "$bad_name");
    assert_eq!((items[0].severity, items[0].line, items[0].hint.as_deref()), (Severity::Error, 2, Some("h")));
    assert_eq!(&TEXT[items[1].range.clone()], "c = 3", "no column: the line's content");
    assert_eq!(items[2].line, 1, "no line: reported on line 1");
    assert_eq!(items[2].severity, Severity::Note);
}

#[test]
fn schema_and_tag_are_checked() {
    let mut d = Diagnostics::default();
    assert_eq!(d.accept(&tag(0), tag(0), TEXT, r#"{"schema_version":1,"diagnostics":[]}"#, &[]), Err(DiagError::UnsupportedSchema(1)));
    assert!(matches!(d.accept(&tag(0), tag(0), TEXT, "not json", &[]), Err(DiagError::BadJson(_))));
    let mut other = tag(0);
    other.cfg = 1;
    assert_eq!(d.accept(&tag(0), other, TEXT, &report(""), &[]), Err(DiagError::StaleTag));
    let mut other = tag(0);
    other.epoch = "x".into();
    assert_eq!(d.accept(&tag(0), other, TEXT, &report(""), &[]), Err(DiagError::StaleTag));
}

#[test]
fn touched_lines_drop_and_untouched_lines_map() {
    let json = report(
        r#"{"code":"E1","severity":"error","file":"-","line":2,"column":3,"message":"m","hint":null},
           {"code":"E2","severity":"error","file":"-","line":3,"column":1,"message":"m","hint":null}"#,
    );
    // A whole line inserted above maps both.
    let mut d = Diagnostics::default();
    let above = delta(vec![Edit { offset: 0, delete: 0, insert: "new line\n".into() }]);
    d.accept(&tag(1), tag(0), TEXT, &json, std::slice::from_ref(&above)).unwrap();
    assert_eq!(d.items().len(), 2);
    let shifted = format!("new line\n{TEXT}");
    assert_eq!(&shifted[d.items()[0].range.clone()], "$bad_name");
    assert_eq!(&shifted[d.items()[1].range.clone()], "c");

    // Typing on line 2 drops line 2's diagnostic only.
    let at = shifted.find("= 2").unwrap();
    d.apply_delta(&delta(vec![Edit { offset: at, delete: 0, insert: "x".into() }]));
    assert_eq!(d.items().len(), 1);
    assert_eq!(d.items()[0].code, "E2");

    // Joining line 3 onto line 2 (deleting the newline before it) maps it.
    let mut d = Diagnostics::default();
    d.accept(&tag(0), tag(0), TEXT, &json, &[]).unwrap();
    let nl = TEXT.find("c = 3").unwrap() - 1;
    d.apply_delta(&delta(vec![Edit { offset: nl, delete: 1, insert: String::new() }]));
    assert_eq!(d.items().iter().map(|i| i.code.as_str()).collect::<Vec<_>>(), ["E2"], "line 2 lost its newline: dropped");
    let joined = TEXT.replacen("2\nc", "2c", 1);
    assert_eq!(&joined[d.items()[0].range.clone()], "c");

    // A resync clears everything.
    d.apply_delta(&ViewDelta { edits: vec![], origin: None, kind: DeltaKind::Resync, rev: 2, view_gen: 2 });
    assert!(d.items().is_empty());
}

fn item(line: usize, code: &str) -> DiagItem {
    DiagItem { line, column: None, severity: Severity::Error, code: code.into(), message: "m".into(), hint: None }
}

fn codes(d: &Diagnostics) -> Vec<(String, String)> {
    d.items().iter().map(|i| (i.source.clone(), i.code.clone())).collect()
}

fn pairs(v: &[(&str, &str)]) -> Vec<(String, String)> {
    v.iter().map(|(s, c)| (s.to_string(), c.to_string())).collect()
}

#[test]
fn accept_items_replaces_only_its_own_source() {
    let lint = report(r#"{"code":"L1","severity":"error","file":"-","line":1,"column":null,"message":"m","hint":null}"#);
    let mut d = Diagnostics::default();
    d.accept(&tag(0), tag(0), TEXT, &lint, &[]).unwrap();
    d.accept_items("scenes", &tag(0), tag(0), TEXT, &[item(2, "S1"), item(3, "S2")], &[]).unwrap();
    d.accept_items("other", &tag(0), tag(0), TEXT, &[item(1, "O1")], &[]).unwrap();
    assert_eq!(codes(&d), pairs(&[("lint", "L1"), ("scenes", "S1"), ("scenes", "S2"), ("other", "O1")]));
    assert_eq!(&TEXT[d.items()[1].range.clone()], "$bad_name = 2", "no column: the line past its indentation");

    // A new lint result keeps the external sets, and stays first.
    let lint = report(r#"{"code":"L2","severity":"warning","file":"-","line":3,"column":1,"message":"m","hint":null}"#);
    d.accept(&tag(0), tag(0), TEXT, &lint, &[]).unwrap();
    assert_eq!(codes(&d), pairs(&[("lint", "L2"), ("scenes", "S1"), ("scenes", "S2"), ("other", "O1")]));

    // Replacing one external set keeps its slot and the others.
    d.accept_items("scenes", &tag(0), tag(0), TEXT, &[item(1, "S3")], &[]).unwrap();
    assert_eq!(codes(&d), pairs(&[("lint", "L2"), ("scenes", "S3"), ("other", "O1")]));

    // An empty set clears only that source; in-process lint through
    // accept_items is the lint set.
    d.accept_items("scenes", &tag(0), tag(0), TEXT, &[], &[]).unwrap();
    d.accept_items(LINT_SOURCE, &tag(0), tag(0), TEXT, &[item(2, "L3")], &[]).unwrap();
    assert_eq!(codes(&d), pairs(&[("lint", "L3"), ("other", "O1")]));

    // Stale tags change nothing.
    let mut other = tag(0);
    other.buffer = "x".into();
    assert_eq!(d.accept_items("other", &tag(0), other, TEXT, &[], &[]), Err(DiagError::StaleTag));
    assert_eq!(d.accept_items("other", &tag(0), tag(1), TEXT, &[], &[]), Err(DiagError::StaleTag), "a gen ahead");
    assert_eq!(codes(&d), pairs(&[("lint", "L3"), ("other", "O1")]));

    // A resync clears every set.
    d.apply_delta(&ViewDelta { edits: vec![], origin: None, kind: DeltaKind::Resync, rev: 2, view_gen: 2 });
    assert!(d.items().is_empty());
}

#[test]
fn external_sets_follow_covered_range_invalidation() {
    let mut d = Diagnostics::default();
    // Located at gen 0, mapped through a whole line inserted above.
    let above = delta(vec![Edit { offset: 0, delete: 0, insert: "new line\n".into() }]);
    d.accept_items("scenes", &tag(1), tag(0), TEXT, &[item(2, "S1"), item(3, "S2")], std::slice::from_ref(&above)).unwrap();
    let shifted = format!("new line\n{TEXT}");
    assert_eq!(&shifted[d.items()[0].range.clone()], "$bad_name = 2");
    assert_eq!(&shifted[d.items()[1].range.clone()], "c = 3");
    // Typing on the first one's line drops it, maps the other.
    let at = shifted.find("= 2").unwrap();
    d.apply_delta(&delta(vec![Edit { offset: at, delete: 0, insert: "x".into() }]));
    assert_eq!(codes(&d), pairs(&[("scenes", "S2")]));
    let typed = "new line\na = 1\n  $bad_name x= 2\nc = 3\n";
    assert_eq!(&typed[d.items()[0].range.clone()], "c = 3");
    assert_eq!(d.items()[0].source, "scenes");
}
