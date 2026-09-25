//! Human selection and paste. The Bus control lane deliberately does not use
//! this encoder: its synthetic-key restrictions remain in `encode_text`.

use super::{Listener, SelectionSide, SelectionType, Terminal};
use rio_vt::{
    crosswords::{
        Crosswords, Mode,
        pos::{Column, Line, Pos},
    },
    selection::Selection,
};
use std::time::Instant;

fn point(term: &Crosswords<Listener>, col: u16, row: u16) -> Pos {
    Pos::new(
        Line(
            row.min(term.screen_lines().saturating_sub(1) as u16) as i32
                - term.display_offset() as i32,
        ),
        Column(usize::from(col).min(term.columns().saturating_sub(1))),
    )
}

impl Terminal {
    /// Viewport cells become signed Rio grid coordinates while holding the
    /// grid lock. Rio owns rotation, history eviction, resize and screen swaps.
    pub fn selection_start(&self, col: u16, row: u16, side: SelectionSide, ty: SelectionType) {
        let mut term = self.grid.lock();
        term.selection = Some(Selection::new(ty, point(&term, col, row), side));
        drop(term);
        self.listener.dirty();
    }

    pub fn selection_update(&self, col: u16, row: u16, side: SelectionSide) {
        let mut term = self.grid.lock();
        let point = point(&term, col, row);
        let Some(selection) = term.selection.as_mut() else {
            return;
        };
        let before = selection.clone();
        selection.update(point, side);
        let changed = *selection != before;
        drop(term);
        if changed {
            self.listener.dirty();
        }
    }

    pub fn selection_clear(&self) {
        let changed = self.grid.lock().selection.take().is_some();
        if changed {
            self.listener.dirty();
        }
    }

    /// A simple click has identical anchors and therefore no selection.
    pub fn selection_finish(&self) -> Option<String> {
        let mut term = self.grid.lock();
        if term.selection.as_ref().is_some_and(Selection::is_empty) {
            term.selection = None;
        }
        term.selection_to_string()
    }

    pub fn selection_text(&self) -> Option<String> {
        self.grid.lock().selection_to_string()
    }

    /// Mode ownership, independent of whether a report was successfully queued
    /// (or an X10 release deliberately produced no report).
    pub fn mouse_reporting(&self) -> bool {
        self.grid.lock().mode().intersects(Mode::MOUSE_MODE)
    }

    /// One atomic human write, through the existing metered PTY queue. Hold
    /// grid then writes, like the parser, so mode sampling and admission agree.
    pub fn paste(&self, text: &str) -> Result<(), String> {
        if text.is_empty() {
            return Ok(());
        }
        let term = self.grid.lock();
        let bytes = encode_paste(text, term.mode().contains(Mode::BRACKETED_PASTE));
        let mut writes = self.listener.writes.lock().unwrap();
        Listener::revoke_writer(&mut writes);
        self.listener
            .enqueue(&mut writes, bytes, Some(Instant::now()), None)?;
        drop(writes);
        drop(term);
        self.listener.follow_input();
        Ok(())
    }
}

fn encode_paste(text: &str, bracketed: bool) -> Vec<u8> {
    if !bracketed {
        return text.replace("\r\n", "\n").replace('\n', "\r").into_bytes();
    }
    // A stack removes both delimiters, including ones exposed by removing an
    // embedded delimiter. Linear time, and all other UTF-8 bytes stay intact.
    let mut payload = Vec::with_capacity(text.len());
    for byte in text.bytes() {
        payload.push(byte);
        if payload.ends_with(b"\x1b[200~") || payload.ends_with(b"\x1b[201~") {
            payload.truncate(payload.len() - 6);
        }
    }
    let mut bytes = b"\x1b[200~".to_vec();
    bytes.extend(payload);
    bytes.extend_from_slice(b"\x1b[201~");
    bytes
}

#[cfg(test)]
mod tests {
    use super::super::MouseModifiers;
    use super::*;
    use rio_vt::{event::Msg, performer::handler::Processor};

    fn select(term: &Terminal, start: (u16, u16), end: (u16, u16)) {
        term.selection_start(start.0, start.1, SelectionSide::Left, SelectionType::Simple);
        term.selection_update(end.0, end.1, SelectionSide::Right);
    }

    #[test]
    fn drag_word_line_wraps_and_trailing_blanks_use_rio_text() {
        let term = Terminal::from_test_vt(8, 4, b"hello world\r\nlast  ");
        select(&term, (0, 0), (7, 1));
        assert_eq!(term.selection_finish().as_deref(), Some("hello world"));
        term.selection_start(1, 1, SelectionSide::Left, SelectionType::Semantic);
        assert_eq!(term.selection_finish().as_deref(), Some("world"));
        term.selection_start(1, 1, SelectionSide::Left, SelectionType::Lines);
        assert_eq!(term.selection_finish().as_deref(), Some("hello world\n"));
        select(&term, (0, 2), (7, 3));
        assert_eq!(term.selection_finish().as_deref(), Some("last"));
        term.selection_start(2, 0, SelectionSide::Left, SelectionType::Simple);
        assert_eq!(
            term.selection_finish(),
            None,
            "a click clears the old selection"
        );
        let other = Terminal::from_test_vt(8, 4, b"other");
        select(&term, (0, 0), (4, 0));
        assert_eq!(
            other.selection_text(),
            None,
            "selections belong to their pane"
        );
    }

    #[test]
    fn selection_tracks_history_and_output_and_expires_after_eviction() {
        let text = (0..8).map(|n| format!("line{n}\r\n")).collect::<String>();
        let term = Terminal::from_test_vt(8, 3, text.as_bytes());
        term.scroll_view(super::super::ScrollRequest::Top);
        select(&term, (0, 0), (4, 0));
        assert_eq!(term.selection_text().as_deref(), Some("line0"));
        term.scroll_view(super::super::ScrollRequest::Bottom);
        assert_eq!(term.selection_text().as_deref(), Some("line0"));
        Processor::default().advance(&mut *term.grid.lock(), b"more\r\n");
        assert_eq!(term.selection_text().as_deref(), Some("line0"));
        Processor::default().advance(&mut *term.grid.lock(), &b"next\r\n".repeat(1010));
        assert_eq!(
            term.selection_text(),
            None,
            "eviction must not select replacement text"
        );
    }

    #[test]
    fn selection_highlight_moves_with_the_viewport_not_the_screen_row() {
        let term = Terminal::from_test_vt(8, 3, b"first\r\nsecond\r\nthird\r\nfourth");
        let live = term.grid_snapshot().screen;
        select(&term, (0, 0), (5, 0));
        assert_eq!(term.selection_text().as_deref(), Some("second"));
        term.grid_snapshot();
        term.scroll_view(super::super::ScrollRequest::Top);
        let history = term.grid_snapshot();
        assert_eq!(history.dirty_rows, [true; 3]);
        assert_eq!(term.selection_text().as_deref(), Some("second"));
        for x in 0..8 {
            let cell = &history.screen.cells[8 + x];
            let base = &live.cells[x];
            assert_eq!(
                (cell.fg, cell.bg),
                if x <= 5 {
                    (base.bg, base.fg)
                } else {
                    (base.fg, base.bg)
                }
            );
        }
        term.scroll_view(super::super::ScrollRequest::Bottom);
        assert_eq!(term.grid_snapshot().dirty_rows, [true; 3]);
        // The other consuming API also consumes selection damage.
        term.selection_clear();
        term.screen(true);
        assert_eq!(term.grid_snapshot().dirty_rows, [false; 3]);
    }

    #[test]
    fn reporting_ownership_includes_x10_even_when_release_is_not_reported() {
        let term = Terminal::from_test_vt(8, 3, b"\x1b[?9h");
        let rx = term.listener.test_input_receiver();
        assert!(term.mouse_reporting());
        assert!(!term.mouse_button(0, 0, 1, false, MouseModifiers::default()));
        assert!(rx.try_recv().is_err());
        let shift = MouseModifiers {
            shift: true,
            ..Default::default()
        };
        assert!(!term.mouse_button(0, 0, 0, true, shift));
        term.selection_start(0, 0, SelectionSide::Left, SelectionType::Lines);
        assert!(term.selection_text().is_some());
        Processor::default().advance(&mut *term.grid.lock(), b"\x1b[?9l");
        assert!(!term.mouse_reporting());
    }

    #[test]
    fn capture_inverts_only_selected_cells_and_damages_old_and_new_rows() {
        let term = Terminal::from_test_vt(8, 4, b"\x1b[31;44mabcdef\r\n\x1b[1;7mghijkl");
        let base = term.grid_snapshot().screen;
        select(&term, (2, 0), (3, 1));
        assert!(term.take_damage());
        let _ = term.screen(false); // A peek must not consume selection damage.
        let selected = term.grid_snapshot();
        assert_eq!(selected.dirty_rows, [true, true, false, false]);
        for (i, (before, after)) in base.cells.iter().zip(&selected.screen.cells).enumerate() {
            let expected = if (2..=11).contains(&i) {
                (before.bg, before.fg)
            } else {
                (before.fg, before.bg)
            };
            assert_eq!((after.fg, after.bg), expected, "cell {i}");
        }
        assert_eq!(term.grid_snapshot().dirty_rows, [false; 4]);
        term.selection_start(0, 2, SelectionSide::Left, SelectionType::Lines);
        assert_eq!(term.grid_snapshot().dirty_rows, [true, true, true, false]);
        term.selection_clear();
        assert_eq!(term.grid_snapshot().dirty_rows, [false, false, true, false]);
        term.selection_start(1, 1, SelectionSide::Left, SelectionType::Lines);
        term.grid_snapshot();
        Processor::default().advance(&mut *term.grid.lock(), b"\x1b[2J");
        assert_eq!(term.selection_text(), None);
        assert!(term.grid_snapshot().dirty_rows[1]);
    }

    #[test]
    fn paste_encoding_keeps_unicode_and_cannot_end_brackets_early() {
        assert_eq!(
            encode_paste("a\r\nb\nc\rdé", false),
            "a\rb\rc\rdé".as_bytes()
        );
        assert_eq!(
            encode_paste("a\r\nb\né", true),
            "\x1b[200~a\r\nb\né\x1b[201~".as_bytes()
        );
        assert_eq!(
            encode_paste("a\x1b[201~b\x1b[200~c\x1b[20\x1b[201~1~d", true),
            b"\x1b[200~abcd\x1b[201~"
        );
    }

    #[test]
    fn human_paste_uses_mode_queue_and_follows_input_without_relaxing_control() {
        let term = Terminal::from_test_vt(8, 3, &b"line\r\n".repeat(8));
        let rx = term.listener.test_input_receiver();
        for bracketed in [false, true] {
            Processor::default().advance(
                &mut *term.grid.lock(),
                if bracketed {
                    b"\x1b[?2004h"
                } else {
                    b"\x1b[?2004l"
                },
            );
            term.scroll_view(super::super::ScrollRequest::Top);
            term.paste("é\ntext").unwrap();
            let Msg::Input(bytes) = rx.try_recv().unwrap() else {
                panic!("expected input")
            };
            assert_eq!(&*bytes, encode_paste("é\ntext", bracketed));
            assert_eq!(term.display_offset(), 0);
        }
        assert!(super::super::encode_text("\x1b[200~text\x1b[201~").is_err());
        term.scroll_view(super::super::ScrollRequest::Top);
        term.paste("").unwrap();
        assert!(term.display_offset() > 0);
        assert!(
            term.paste(&"x".repeat(65536)).is_err(),
            "queue refuses a whole oversized paste"
        );
        assert!(term.display_offset() > 0);
    }
}
