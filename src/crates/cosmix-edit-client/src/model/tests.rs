//! EditorModel against real `Text` (plan §7.1): every command, multi-item
//! Tab/Outdent, delta mapping bias, composition cancel/map.

use cosmix_edit_core::anchor::Selection;
use cosmix_edit_core::ot::{Edit, txn_sequence};
use cosmix_edit_core::text::Text;
use cosmix_edit_core::view::MeasureCfg;

use super::*;
use crate::types::{DeltaKind, LocalEdit, ViewDelta};

fn cfg() -> EditCfg {
    EditCfg { measure: MeasureCfg { tab_size: 4, ambiguous_wide: false }, insert_spaces: true, eol: "\n", line_comment: Some("--") }
}

fn model(anchor: usize, head: usize) -> EditorModel {
    EditorModel { sel: Selection { anchor, head }, ..EditorModel::default() }
}

/// Apply a LocalEdit's items (base coords) to a string, as the mirror would.
fn apply(s: &str, e: &LocalEdit) -> String {
    let mut out = s.to_string();
    for (_, edit) in txn_sequence(&e.items).expect("non-overlapping") {
        out.replace_range(edit.offset..edit.offset + edit.delete, &edit.insert);
    }
    out
}

fn run(s: &str, m: &mut EditorModel, c: EditCommand) -> (String, LocalEdit) {
    run_cfg(s, m, &cfg(), c)
}

fn run_cfg(s: &str, m: &mut EditorModel, cfg: &EditCfg, c: EditCommand) -> (String, LocalEdit) {
    let text = Text::from_text(s).unwrap();
    let e = m.command(&text, cfg, c).expect("an edit");
    (apply(s, &e), e)
}

fn motion(s: &str, m: &mut EditorModel, to: Motion, extend: bool) {
    let text = Text::from_text(s).unwrap();
    assert!(m.command(&text, &cfg(), EditCommand::Move { to, extend }).is_none());
}

#[test]
fn insert_replaces_selection_and_coalesces_single_chars() {
    let mut m = model(1, 3);
    let (s, e) = run("abcd", &mut m, EditCommand::Insert("X".into()));
    assert_eq!(s, "aXd");
    assert!(!e.coalesce, "replacing a selection starts a new undo group");
    assert_eq!(e.caret_after, Selection { anchor: 2, head: 2 });
    let mut m = model(2, 2);
    let (s, e) = run("abcd", &mut m, EditCommand::Insert("é".into()));
    assert_eq!(s, "abécd");
    assert!(e.coalesce);
    let (_, e) = run("abcd", &mut model(2, 2), EditCommand::Insert("xy".into()));
    assert!(!e.coalesce, "a paste is not typing");
    let (s, e) = run("abcd", &mut model(2, 2), EditCommand::Insert("e\u{301}".into()));
    assert_eq!(s, "abe\u{301}cd");
    assert!(e.coalesce, "one grapheme of two scalars is one keystroke");
    let (_, e) = run("abcd", &mut model(2, 2), EditCommand::Insert("\r\n".into()));
    assert!(!e.coalesce, "a CRLF Enter starts a new undo group");
}

#[test]
fn overwrite_replaces_the_next_grapheme_but_not_the_line_end() {
    let mut m = model(1, 1);
    m.overwrite = true;
    let (s, _) = run("ab\ncd", &mut m, EditCommand::Insert("X".into()));
    assert_eq!(s, "aX\ncd");
    let mut m = model(2, 2);
    m.overwrite = true;
    let (s, _) = run("ab\ncd", &mut m, EditCommand::Insert("X".into()));
    assert_eq!(s, "abX\ncd", "at the line end overwrite inserts");
}

#[test]
fn newline_auto_indents_with_the_buffer_eol() {
    let mut m = model(9, 9);
    let (s, e) = run("x\n    abcd", &mut m, EditCommand::Newline);
    assert_eq!(s, "x\n    abc\n    d");
    assert_eq!(e.caret_after.head, 9 + 5);
    let crlf = EditCfg { eol: "\r\n", ..cfg() };
    let (s, _) = run_cfg("\tab", &mut model(3, 3), &crlf, EditCommand::Newline);
    assert_eq!(s, "\tab\r\n\t");
    let (s, _) = run("  ab", &mut model(1, 1), EditCommand::Newline);
    assert_eq!(s, " \n  ab", "indent never copies past the caret");
}

#[test]
fn backspace_and_delete_are_grapheme_correct() {
    let family = "a👨\u{200d}👩\u{200d}👧b";
    let after_family = 1 + "👨\u{200d}👩\u{200d}👧".len();
    let (s, e) = run(family, &mut model(after_family, after_family), EditCommand::Backspace);
    assert_eq!(s, "ab");
    assert!(e.coalesce);
    let (s, _) = run(family, &mut model(1, 1), EditCommand::Delete);
    assert_eq!(s, "ab");
    let (s, _) = run("x\r\ny", &mut model(3, 3), EditCommand::Backspace);
    assert_eq!(s, "xy", "CRLF is one cluster");
    let text = Text::from_text("ab").unwrap();
    assert!(model(0, 0).command(&text, &cfg(), EditCommand::Backspace).is_none());
    assert!(model(2, 2).command(&text, &cfg(), EditCommand::Delete).is_none());
    let (s, e) = run("abcd", &mut model(3, 1), EditCommand::Backspace);
    assert_eq!(s, "ad");
    assert!(!e.coalesce);
}

#[test]
fn delete_word_left_and_right() {
    let (s, _) = run("foo bar baz", &mut model(7, 7), EditCommand::DeleteWordLeft);
    assert_eq!(s, "foo  baz");
    let (s, _) = run("foo bar baz", &mut model(4, 4), EditCommand::DeleteWordRight);
    assert!(s == "foo  baz" || s == "foo baz", "{s}");
}

#[test]
fn tab_inserts_to_the_next_stop_or_a_tab() {
    let (s, e) = run("ab", &mut model(1, 1), EditCommand::Tab);
    assert_eq!(s, "a   b");
    assert!(!e.coalesce);
    let tabs = EditCfg { insert_spaces: false, ..cfg() };
    let (s, e) = run_cfg("ab", &mut model(1, 1), &tabs, EditCommand::Tab);
    assert_eq!(s, "a\tb");
    assert!(e.coalesce);
}

#[test]
fn multi_line_tab_and_outdent_are_one_multi_item_edit() {
    let src = "one\n\ntwo\nthree\n";
    // Selection from inside line 1 to the start of line 4: lines 1..=3.
    let mut m = model(1, 9);
    let (s, e) = run(src, &mut m, EditCommand::Tab);
    assert_eq!(e.items.len(), 2, "empty lines are not indented: {:?}", e.items);
    assert_eq!(s, "    one\n\n    two\nthree\n");
    assert_eq!(e.caret_after, Selection { anchor: 5, head: 17 });
    let mut m = model(0, 17);
    let (back, e) = run(&s, &mut m, EditCommand::Outdent);
    assert_eq!(e.items.len(), 2);
    assert_eq!(back, src);
    let (s, _) = run("\t\tx\n  y", &mut model(0, 7), EditCommand::Outdent);
    assert_eq!(s, "\tx\ny");
    let text = Text::from_text("x\ny").unwrap();
    assert!(model(0, 3).command(&text, &cfg(), EditCommand::Outdent).is_none());
}

#[test]
fn duplicate_delete_and_move_lines() {
    let (s, e) = run("ab\ncd", &mut model(1, 1), EditCommand::DuplicateLine);
    assert_eq!(s, "ab\nab\ncd");
    assert_eq!(e.caret_after.head, 4);
    let crlf = EditCfg { eol: "\r\n", ..cfg() };
    let (s, _) = run_cfg("ab\r\ncd", &mut model(1, 1), &crlf, EditCommand::DuplicateLine);
    assert_eq!(s, "ab\r\nab\r\ncd");
    let (s, _) = run("abcd", &mut model(1, 3), EditCommand::DuplicateLine);
    assert_eq!(s, "abcbcd");

    let (s, _) = run("a\nb\nc", &mut model(2, 2), EditCommand::DeleteLine);
    assert_eq!(s, "a\nc");
    let (s, _) = run("a\nb\nc", &mut model(4, 4), EditCommand::DeleteLine);
    assert_eq!(s, "a\nb");
    let (s, _) = run("a\nb", &mut model(0, 3), EditCommand::DeleteLine);
    assert_eq!(s, "");

    let mut m = model(3, 3);
    let (s, e) = run("a\nbb\nc", &mut m, EditCommand::MoveLineUp);
    assert_eq!(s, "bb\na\nc");
    assert_eq!(e.caret_after.head, 1);
    let (s, e) = run("a\nbb\nc", &mut model(3, 3), EditCommand::MoveLineDown);
    assert_eq!(s, "a\nc\nbb");
    assert_eq!(&s[e.caret_after.head - 1..e.caret_after.head], "b");
    let (s, e) = run("a\nbb", &mut model(3, 3), EditCommand::MoveLineUp);
    assert_eq!(s, "bb\na", "the last line gains the eol it moves past");
    assert_eq!(e.caret_after.head, 1);
    let text = Text::from_text("a\nb").unwrap();
    assert!(model(0, 0).command(&text, &cfg(), EditCommand::MoveLineUp).is_none());
    assert!(model(2, 2).command(&text, &cfg(), EditCommand::MoveLineDown).is_none());
}


#[test]
fn toggle_comment_adds_at_the_common_indent_and_removes() {
    let src = "  a\n\n    b\n";
    let mut m = model(0, 10);
    let (s, e) = run(src, &mut m, EditCommand::ToggleComment);
    assert_eq!(e.items.len(), 2, "blank lines are left alone");
    assert_eq!(s, "  -- a\n\n  --   b\n");
    let (back, _) = run(&s, &mut model(0, s.len()), EditCommand::ToggleComment);
    assert_eq!(back, src);
    let (s, _) = run("--x", &mut model(0, 0), EditCommand::ToggleComment);
    assert_eq!(s, "x", "a token with no space after it is removed alone");
    let text = Text::from_text("x").unwrap();
    let none = EditCfg { line_comment: None, ..cfg() };
    assert!(model(0, 0).command(&text, &none, EditCommand::ToggleComment).is_none());
}

#[test]
fn motions_are_grapheme_correct_and_collapse_selections() {
    let s = "ab👍🏽c";
    let mut m = model(2, 2);
    motion(s, &mut m, Motion::Right, false);
    assert_eq!(m.sel.head, 2 + "👍🏽".len(), "a skin-tone emoji is one cluster");
    motion(s, &mut m, Motion::Left, false);
    assert_eq!(m.sel.head, 2);
    motion(s, &mut m, Motion::Right, true);
    assert_eq!((m.sel.anchor, m.sel.head), (2, 10));
    motion(s, &mut m, Motion::Left, false);
    assert_eq!((m.sel.anchor, m.sel.head), (2, 2), "Left collapses a selection to its start");
    let mut m = model(0, 1);
    motion(s, &mut m, Motion::Right, false);
    assert_eq!(m.sel.head, 1);
    motion(s, &mut m, Motion::DocEnd, false);
    assert_eq!(m.sel.head, s.len());
    motion(s, &mut m, Motion::DocStart, true);
    assert_eq!((m.sel.anchor, m.sel.head), (s.len(), 0));
    let mut m = model(0, 0);
    motion("x\r\ny", &mut m, Motion::End, false);
    assert_eq!(m.sel.head, 1, "End stops before a CRLF");
}

#[test]
fn home_is_smart_and_vertical_motion_keeps_the_column() {
    let mut m = model(8, 8);
    motion("x\n    abcd", &mut m, Motion::Home, false);
    assert_eq!(m.sel.head, 6, "first to the first non-blank");
    motion("x\n    abcd", &mut m, Motion::Home, false);
    assert_eq!(m.sel.head, 2, "then to column 1");
    motion("x\n    abcd", &mut m, Motion::Home, false);
    assert_eq!(m.sel.head, 6);

    let s = "abcdef\nab\nabcdef";
    let mut m = model(5, 5);
    motion(s, &mut m, Motion::Down, false);
    assert_eq!(m.sel.head, 9, "clamped to the short line's end");
    motion(s, &mut m, Motion::Down, false);
    assert_eq!(m.sel.head, 15, "the preferred column comes back");
    motion(s, &mut m, Motion::Down, false);
    assert_eq!(m.sel.head, s.len(), "Down on the last line goes to the end");
    motion(s, &mut m, Motion::Up, false);
    motion(s, &mut m, Motion::Up, false);
    motion(s, &mut m, Motion::Up, false);
    assert_eq!(m.sel.head, 0, "Up on the first line goes to the start");
    let wide = "中文x\nabcdef";
    let mut m = model(6, 6); // after 中文: 4 cells
    motion(wide, &mut m, Motion::Down, false);
    assert_eq!(m.sel.head, 8 + 4);
    let mut m = model(0, 0);
    m.scroll.first_line = 1;
    motion("a\nb\nc\nd\ne", &mut m, Motion::PageDown(2), false);
    assert_eq!(m.sel.head, 4);
    assert_eq!(m.scroll.first_line, 3);
    motion("a\nb\nc\nd\ne", &mut m, Motion::PageUp(5), false);
    assert_eq!((m.sel.head, m.scroll.first_line), (0, 1));
}

#[test]
fn select_word_line_all_and_set_selection() {
    let s = "foo bar\nbaz";
    let text = Text::from_text(s).unwrap();
    let mut m = model(0, 0);
    m.command(&text, &cfg(), EditCommand::SelectWord(5));
    assert_eq!((m.sel.anchor, m.sel.head), (4, 7));
    m.command(&text, &cfg(), EditCommand::SelectLine(2));
    assert_eq!((m.sel.anchor, m.sel.head), (0, 8), "a line selection includes its newline");
    m.command(&text, &cfg(), EditCommand::SelectAll);
    assert_eq!((m.sel.anchor, m.sel.head), (0, s.len()));
    let t = Text::from_text("é").unwrap();
    m.command(&t, &cfg(), EditCommand::SetSelection(Selection { anchor: 1, head: 99 }));
    assert_eq!((m.sel.anchor, m.sel.head), (0, 2), "clamped to char boundaries");
}

fn delta(kind: DeltaKind, edits: Vec<Edit>, origin: Option<&str>) -> ViewDelta {
    ViewDelta { edits, origin: origin.map(|o| o.parse().unwrap()), kind, rev: 7, view_gen: 3 }
}

fn ins(offset: usize, s: &str) -> Edit {
    Edit { offset, delete: 0, insert: s.into() }
}

#[test]
fn own_selection_maps_after_for_local_and_before_otherwise() {
    let mut m = model(4, 4);
    m.apply_delta(&delta(DeltaKind::Local, vec![ins(4, "xy")], Some("human:ced")));
    assert_eq!(m.sel.head, 6);
    m.apply_delta(&delta(DeltaKind::Remote, vec![ins(6, "zz")], Some("agent:ctl-90")));
    assert_eq!(m.sel.head, 6, "a remote insert at the caret goes after it");
    m.remote = vec![("agent:a".parse().unwrap(), vec![Selection { anchor: 2, head: 2 }])];
    m.apply_delta(&delta(DeltaKind::Local, vec![ins(2, "q")], Some("human:ced")));
    assert_eq!(m.remote[0].1[0].head, 2, "remote carets map Before");
    m.apply_delta(&delta(DeltaKind::Remote, vec![Edit { offset: 0, delete: 3, insert: String::new() }], Some("agent:a")));
    assert_eq!(m.sel.head, 4);
}

#[test]
fn markers_record_other_origins_and_follow_the_text() {
    let mut m = model(0, 0);
    m.apply_delta(&delta(DeltaKind::Local, vec![ins(0, "mine")], Some("human:ced")));
    assert!(m.markers.changed.is_empty(), "own UI edits are not marked");
    m.apply_delta(&delta(DeltaKind::Remote, vec![ins(10, "agent")], Some("agent:ctl-90")));
    assert_eq!(m.markers.changed.len(), 1);
    assert_eq!(m.markers.changed[0].0, 10..15);
    assert_eq!(m.markers.changed[0].2, 7);
    m.apply_delta(&delta(DeltaKind::Remote, vec![ins(0, "ab")], Some("agent:other")));
    assert_eq!(m.markers.changed[0].0, 12..17, "markers map through later edits");
    m.apply_delta(&delta(DeltaKind::Local, vec![ins(0, "x")], Some("agent:ced.local_ctl")));
    assert_eq!(m.markers.changed.len(), 3, "Bus-driven ced edits are marked");
    m.apply_delta(&delta(DeltaKind::Reload, vec![ins(0, "r")], Some("tool:disk")));
    assert_eq!(m.markers.changed.len(), 3, "reloads are not marked");
    m.clear_markers();
    assert!(m.markers.changed.is_empty());
}

#[test]
fn composition_is_cancelled_by_overlap_and_mapped_otherwise() {
    let mut m = model(5, 5);
    m.set_preedit(true);
    assert_eq!(m.composition, Some(5..5));
    m.apply_delta(&delta(DeltaKind::Remote, vec![ins(0, "ab")], Some("agent:a")));
    assert_eq!(m.composition, Some(7..7), "an edit before it maps it");
    m.apply_delta(&delta(DeltaKind::Remote, vec![ins(7, "zz")], Some("agent:a")));
    assert_eq!(m.composition, Some(7..7), "an insert at its point abuts it");
    m.apply_delta(&delta(DeltaKind::Remote, vec![Edit { offset: 7, delete: 2, insert: String::new() }], Some("agent:a")));
    assert_eq!(m.composition, Some(7..7), "a delete starting at its point abuts it");
    m.apply_delta(&delta(DeltaKind::Remote, vec![Edit { offset: 6, delete: 2, insert: String::new() }], Some("agent:a")));
    assert_eq!(m.composition, None, "a delete spanning it cancels it");

    let mut m = model(2, 6);
    m.set_preedit(true);
    assert_eq!(m.composition, Some(2..6), "anchored at the selection the commit replaces");
    m.apply_delta(&delta(DeltaKind::Remote, vec![ins(4, "x")], Some("agent:a")));
    assert_eq!(m.composition, None, "an insert inside it cancels it");
    m.set_preedit(true);
    m.set_preedit(false);
    assert_eq!(m.composition, None);
}

#[test]
fn resync_then_clamp_keeps_offsets_valid() {
    let mut m = model(10, 20);
    m.set_preedit(true);
    m.markers.changed.push((15..30, "agent:a".parse().unwrap(), 1));
    m.scroll.first_line = 50;
    m.apply_delta(&delta(DeltaKind::Resync, vec![], None));
    assert_eq!(m.composition, None);
    let text = Text::from_text("héllo\nx").unwrap();
    m.clamp(&text);
    assert_eq!((m.sel.anchor, m.sel.head), (8, 8));
    assert_eq!(m.markers.changed[0].0, 8..8);
    assert_eq!(m.scroll.first_line, 2);
    let mut m = model(2, 2);
    m.clamp(&text);
    assert_eq!(m.sel.head, 1, "moved back off a UTF-8 continuation byte");
}

#[test]
fn line_of_matches_the_line_index() {
    let text = Text::from_text("a\nbb\n\nccc").unwrap();
    let lines: Vec<usize> = (0..=text.len()).map(|o| line_of(&text, o)).collect();
    assert_eq!(lines, [1, 1, 2, 2, 2, 3, 4, 4, 4, 4]);
    assert_eq!(content_end(&Text::from_text("ab\r\ncd").unwrap(), 1), 2);
    assert_eq!(line_comment_for("mix-data"), Some("--"));
    assert_eq!(line_comment_for("markdown"), None);
}
