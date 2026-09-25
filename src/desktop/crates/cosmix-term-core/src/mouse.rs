// Mouse encoding and mode gates ported from rio librio (932c1a7).
// MIT License
// Copyright (c) 2022-present Raphael Amorim
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all
// copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
// SOFTWARE.

use super::{Listener, Terminal};
use rio_vt::crosswords::{Mode, grid::Scroll};
use std::time::Instant;

#[derive(Clone, Copy, Default)]
pub struct MouseModifiers {
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
}
impl MouseModifiers {
    fn bits(self) -> u8 {
        u8::from(self.alt) * 8 + u8::from(self.ctrl) * 16
    }
}

fn mouse_report(button: u8, col: u16, row: u16, pressed: bool, sgr: bool, utf8: bool) -> Vec<u8> {
    let x = col.saturating_add(1);
    let y = row.saturating_add(1);
    if sgr {
        let end = if pressed { 'M' } else { 'm' };
        return format!("\x1b[<{button};{x};{y}{end}").into_bytes();
    }
    let encoded = if pressed { button } else { button | 3 };
    let mut out = vec![0x1b, b'[', b'M', 32u8.saturating_add(encoded)];
    for value in [x, y] {
        if utf8 && value >= 95 {
            let encoded = char::from_u32(32 + value as u32).unwrap_or('\u{20}');
            let mut buffer = [0u8; 4];
            out.extend_from_slice(encoded.encode_utf8(&mut buffer).as_bytes());
        } else {
            out.push(32u8.saturating_add(value.min(223) as u8));
        }
    }
    out
}

impl Terminal {
    fn write_mouse(&self, bytes: Vec<u8>) -> bool {
        let mut writes = self.listener.writes.lock().unwrap();
        // Mouse input has the same human-input authority as keyboard input.
        Listener::revoke_writer(&mut writes);
        match self
            .listener
            .enqueue(&mut writes, bytes, Some(Instant::now()), None)
        {
            Ok(()) => {
                drop(writes);
                self.listener.follow_input();
                true
            },
            Err(e) => {
                eprintln!("PTY mouse input failed: {e}");
                false
            }
        }
    }

    /// Zero-based cell; button 0/1/2 = left/middle/right. Shift bypasses reporting.
    pub fn mouse_button(
        &self,
        col: u16,
        row: u16,
        button: u8,
        pressed: bool,
        mods: MouseModifiers,
    ) -> bool {
        let mode = self.grid.lock().mode();
        if !mode.intersects(Mode::MOUSE_MODE) || mods.shift {
            return false;
        }
        let x10 = mode.contains(Mode::MOUSE_REPORT_X10);
        if x10 && (!pressed || button > 2) {
            return false;
        }
        let encoded = button + if x10 { 0 } else { mods.bits() };
        self.write_mouse(mouse_report(
            encoded,
            col,
            row,
            pressed,
            mode.contains(Mode::SGR_MOUSE),
            mode.contains(Mode::UTF8_MOUSE),
        ))
    }

    /// Button 3 means no button held; modes 1002/1003 select drag/all motion.
    pub fn mouse_motion(&self, col: u16, row: u16, button: u8, mods: MouseModifiers) -> bool {
        let mode = self.grid.lock().mode();
        let wanted = if button >= 3 {
            mode.contains(Mode::MOUSE_MOTION)
        } else {
            mode.intersects(Mode::MOUSE_DRAG | Mode::MOUSE_MOTION)
        };
        if mods.shift || !wanted {
            return false;
        }
        self.write_mouse(mouse_report(
            button.saturating_add(32) + mods.bits(),
            col,
            row,
            true,
            mode.contains(Mode::SGR_MOUSE),
            mode.contains(Mode::UTF8_MOUSE),
        ))
    }

    /// Positive lines scroll up. Returns false when the host should scroll locally.
    pub fn mouse_scroll(&self, col: u16, row: u16, lines: i32, mods: MouseModifiers) -> bool {
        let mode = self.grid.lock().mode();
        if lines == 0 || mods.shift || !mode.intersects(Mode::MOUSE_MODE) {
            return false;
        }
        let button = if lines > 0 { 64 } else { 65 };
        let report = mouse_report(
            button + mods.bits(),
            col,
            row,
            true,
            mode.contains(Mode::SGR_MOUSE),
            mode.contains(Mode::UTF8_MOUSE),
        );
        let mut sent = false;
        for _ in 0..lines.unsigned_abs() {
            if !self.write_mouse(report.clone()) {
                break;
            }
            sent = true;
        }
        sent
    }

    /// Rio's alternate-screen cursor-key fallback, otherwise local scrollback.
    pub fn scroll_wheel(&self, lines: i32, mods: MouseModifiers) {
        if lines == 0 {
            return;
        }
        let mode = self.grid.lock().mode();
        if !mods.shift && mode.contains(Mode::ALT_SCREEN | Mode::ALTERNATE_SCROLL) {
            let seq: &[u8] = match (mode.contains(Mode::APP_CURSOR), lines > 0) {
                (true, true) => b"\x1bOA",
                (true, false) => b"\x1bOB",
                (false, true) => b"\x1b[A",
                (false, false) => b"\x1b[B",
            };
            for _ in 0..lines.unsigned_abs() {
                if !self.write_mouse(seq.to_vec()) {
                    break;
                }
            }
        } else {
            self.grid.lock().scroll_display(Scroll::Delta(lines));
            self.listener.dirty();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use super::*;
    use rio_vt::performer::handler::Processor;

    fn fixture(sequence: &[u8]) -> (Terminal, channel::Receiver<Msg>) {
        let (damage, rx) = mpsc::sync_channel(1);
        let (sender, receiver) = channel::channel();
        let stats = Arc::new(Mutex::new(Metrics::default()));
        let listener = Listener {
            grid: Arc::new(OnceLock::new()),
            damage,
            wake: Arc::new(OnceLock::new()),
            writes: Arc::new(Mutex::new(Writes {
                sender: Some(sender),
                ..Writes::default()
            })),
            stats: stats.clone(),
            quit: Arc::new(AtomicBool::new(false)),
            title: Arc::new(Mutex::new(String::new())),
            title_changed: Arc::new(OnceLock::new()),
        };
        let mut grid = Crosswords::new(
            CrosswordsSize::new(80, 24),
            CursorShape::Block,
            listener.clone(),
            WindowId::from(0),
            0,
            100,
        );
        Processor::default().advance(&mut grid, sequence);
        let grid = Arc::new(FairMutex::new(grid));
        let _ = listener.grid.set(Arc::downgrade(&grid));
        (
            Terminal {
                before_pty_cleanup: None,
                session: None,
                listener,
                stats,
                grid,
                captured_offset: Mutex::new(0),
                damage: Mutex::new(rx),
                pid: 0,
                thread: None,
            },
            receiver,
        )
    }

    fn input(receiver: &channel::Receiver<Msg>) -> Vec<u8> {
        match receiver.try_recv().unwrap() {
            Msg::Input(bytes) => bytes.into_owned(),
            _ => panic!("expected PTY input"),
        }
    }

    fn history() -> (Terminal, channel::Receiver<Msg>) {
        let mut bytes = Vec::new();
        for line in 0..80 {
            bytes.extend_from_slice(format!("line {line}\r\n").as_bytes());
        }
        fixture(&bytes)
    }

    #[test]
    fn viewport_changes_repaint_all_rows_including_return_to_bottom() {
        let (term, _rx) = history();
        let live = term.grid_snapshot().screen;
        term.scroll_wheel(5, MouseModifiers::default());
        assert_eq!(term.grid.lock().display_offset(), 5);
        assert!(term.grid_snapshot().dirty_rows.iter().all(|dirty| *dirty));
        term.scroll_wheel(-5, MouseModifiers::default());
        // A read-only snapshot must not consume the viewport transition.
        let _ = term.screen(false);
        let bottom = term.grid_snapshot();
        assert_eq!(term.grid.lock().display_offset(), 0);
        assert!(bottom.dirty_rows.iter().all(|dirty| *dirty));
        assert_eq!(
            bottom.screen.cells.iter().map(|cell| cell.c).collect::<String>(),
            live.cells.iter().map(|cell| cell.c).collect::<String>(),
        );
        assert!(term.grid_snapshot().dirty_rows.iter().all(|dirty| !dirty));
    }

    #[test]
    fn scroll_and_history_clear_pixels_match_a_fresh_render() {
        use crate::{config::Cursor, raster::{Raster, Surface}};
        let (term, _rx) = history();
        let mut raster = Raster::new(1.0, 13.0, Cursor::Block).unwrap();
        let mut incremental = Surface::default();
        let mut check = || {
            let snapshot = term.grid_snapshot();
            raster.render_into(&snapshot.screen, &snapshot.dirty_rows, &mut incremental);
            let mut fresh = Surface::default();
            raster.render_into(&snapshot.screen, &vec![true; snapshot.screen.rows], &mut fresh);
            assert_eq!(incremental.rgba(), fresh.rgba());
        };
        check();
        term.scroll_wheel(5, MouseModifiers::default());
        check();
        term.scroll_wheel(-5, MouseModifiers::default());
        check();
        term.scroll_view(ScrollRequest::Top);
        check();
        // Bypass Crosswords::scroll_display and its full-damage side effect:
        // only captured_offset detects this return to the live viewport.
        term.grid.lock().grid.clear_history();
        assert_eq!(term.display_offset(), 0);
        check();
    }

    #[test]
    fn snapshot_stays_live_and_cursor_tracks_the_viewport() {
        let (term, _rx) = history();
        let live = term.snapshot();
        let live_text = live.split("--- screen ---\n").nth(1).unwrap();
        let cursor = term.screen(false).cursor;
        term.scroll_wheel(5, MouseModifiers::default());
        let screen = term.screen(false);
        assert_eq!(screen.cursor.1, cursor.1 + 5);
        assert!(!screen.cursor_visible);
        let snapshot = term.snapshot();
        assert_eq!(snapshot.split("--- screen ---\n").nth(1).unwrap(), live_text);
        assert_eq!(snapshot.lines().next(), live.lines().next());
        assert_eq!(term.display_offset(), 5, "snapshot leaves the human viewport alone");
        assert!(term.grid_snapshot().dirty_rows.iter().all(|dirty| *dirty));
        Processor::default().advance(&mut *term.grid.lock(), b"\x1b[1;1H");
        let screen = term.screen(false);
        assert_eq!(screen.cursor, (0, 5));
        assert!(screen.cursor_visible, "a live row still in the viewport keeps its cursor");
    }

    #[test]
    fn listener_text_and_mouse_reports_follow_live_output() {
        let (term, rx) = history();
        term.scroll_view(ScrollRequest::Top);
        term.listener.type_text("pasted\ntext").unwrap();
        assert_eq!(input(&rx), b"pasted\rtext");
        assert_eq!(term.display_offset(), 0);
        term.scroll_view(ScrollRequest::Top);
        term.listener.key(Key::Char('x'), Instant::now()).unwrap();
        assert_eq!(input(&rx), b"x");
        assert_eq!(term.display_offset(), 0, "direct listener keys follow too");
        term.scroll_view(ScrollRequest::Top);
        term.listener.type_text("").unwrap();
        assert!(term.listener.type_text("é").is_err());
        assert_eq!(term.display_offset(), 57);
        Processor::default().advance(&mut *term.grid.lock(), b"\x1b[?1003;1006h");
        for kind in 0..3 {
            term.scroll_view(ScrollRequest::Top);
            let sent = match kind {
                0 => term.mouse_button(2, 3, 0, true, MouseModifiers::default()),
                1 => term.mouse_motion(2, 3, 3, MouseModifiers::default()),
                _ => term.mouse_scroll(2, 3, 1, MouseModifiers::default()),
            };
            assert!(sent);
            assert_eq!(input(&rx), match kind {
                0 => &b"\x1b[<0;3;4M"[..],
                1 => &b"\x1b[<35;3;4M"[..],
                _ => &b"\x1b[<64;3;4M"[..],
            });
            assert_eq!(term.display_offset(), 0);
        }
        term.listener.quit.store(true, Ordering::Release);
        term.scroll_view(ScrollRequest::Top);
        assert!(term.listener.type_text("rejected").is_err());
        assert_eq!(term.display_offset(), 57);
    }

    #[test]
    fn scroll_view_pages_overlap_and_top_bottom_reach_history_edges() {
        let (term, _rx) = history();
        term.scroll_view(ScrollRequest::PageUp);
        assert_eq!(term.grid.lock().display_offset(), 23);
        term.scroll_view(ScrollRequest::PageUp);
        assert_eq!(term.grid.lock().display_offset(), 46);
        term.scroll_view(ScrollRequest::PageDown);
        assert_eq!(term.grid.lock().display_offset(), 23);
        term.scroll_view(ScrollRequest::Top);
        assert_eq!(term.grid.lock().display_offset(), 57);
        term.scroll_view(ScrollRequest::PageUp);
        assert_eq!(term.grid.lock().display_offset(), 57);
        term.scroll_view(ScrollRequest::Bottom);
        assert_eq!(term.grid.lock().display_offset(), 0);
        term.scroll_view(ScrollRequest::PageDown);
        assert_eq!(term.grid.lock().display_offset(), 0);
    }

    #[test]
    fn shell_keys_snap_to_bottom_but_empty_keys_and_vt_replies_do_not() {
        let (term, rx) = history();
        for key in [Key::Char('x'), Key::Enter, Key::PageUp, Key::Control('c')] {
            term.scroll_view(ScrollRequest::Top);
            term.grid_snapshot();
            term.key(key, Instant::now()).unwrap();
            assert_eq!(input(&rx), encode(key));
            assert_eq!(term.grid.lock().display_offset(), 0);
            assert!(term.grid_snapshot().dirty_rows.iter().all(|dirty| *dirty));
            assert!(term.grid_snapshot().dirty_rows.iter().all(|dirty| !dirty));
        }
        term.scroll_view(ScrollRequest::Top);
        term.key(Key::Char('é'), Instant::now()).unwrap();
        assert_eq!(term.grid.lock().display_offset(), 57);
        assert!(rx.try_recv().is_err());
        term.listener.write(b"reply".to_vec(), None).unwrap();
        assert_eq!(input(&rx), b"reply");
        assert_eq!(term.grid.lock().display_offset(), 57);
    }

    #[test]
    fn shift_wheel_bypasses_reporting_and_alternate_scroll_uses_cursor_keys() {
        let (term, rx) = history();
        Processor::default().advance(&mut *term.grid.lock(), b"\x1b[?1000;1006h");
        let shift = MouseModifiers { shift: true, ..Default::default() };
        assert!(!term.mouse_scroll(2, 3, 4, shift));
        term.scroll_wheel(4, shift);
        assert_eq!(term.grid.lock().display_offset(), 4);
        assert!(rx.try_recv().is_err());
        Processor::default().advance(&mut *term.grid.lock(), b"\x1b[?1000l\x1b[?1049;1007h");
        assert!(!term.mouse_scroll(2, 3, 1, MouseModifiers::default()));
        term.scroll_wheel(1, MouseModifiers::default());
        assert_eq!(input(&rx), b"\x1b[A");
        term.scroll_wheel(1, shift);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn sgr_parser_modes_deliver_click_drag_release_and_wheel() {
        let (term, rx) = fixture(b"\x1b[?1002;1006h");
        let mods = MouseModifiers {
            alt: true,
            ctrl: true,
            ..Default::default()
        };
        assert!(term.mouse_button(9, 4, 0, true, mods));
        assert_eq!(input(&rx), b"\x1b[<24;10;5M");
        assert!(term.mouse_motion(10, 5, 0, mods));
        assert_eq!(input(&rx), b"\x1b[<56;11;6M");
        assert!(term.mouse_button(10, 5, 0, false, mods));
        assert_eq!(input(&rx), b"\x1b[<24;11;6m");
        assert!(!term.mouse_motion(10, 5, 3, mods));
        assert!(term.mouse_scroll(9, 4, 2, mods));
        for _ in 0..2 {
            assert_eq!(input(&rx), b"\x1b[<88;10;5M");
        }
        assert!(term.mouse_scroll(9, 4, -1, mods));
        assert_eq!(input(&rx), b"\x1b[<89;10;5M");
        let shift = MouseModifiers {
            shift: true,
            ..mods
        };
        assert!(!term.mouse_button(0, 0, 0, true, shift));
        assert!(!term.mouse_motion(0, 0, 0, shift));
        assert!(!term.mouse_scroll(0, 0, 1, shift));
        Processor::default().advance(&mut *term.grid.lock(), b"\x1b[?1002;1006l");
        assert!(!term.mouse_button(0, 0, 0, true, mods));
        assert!(!term.mouse_scroll(0, 0, 1, mods));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn x10_normal_and_any_motion_gates() {
        let mods = MouseModifiers {
            alt: true,
            ctrl: true,
            ..Default::default()
        };
        let (term, rx) = fixture(b"\x1b[?9h");
        assert!(term.mouse_button(0, 0, 2, true, mods));
        assert_eq!(input(&rx), b"\x1b[M\x22!!");
        assert!(!term.mouse_button(0, 0, 2, false, mods));
        assert!(!term.mouse_motion(0, 0, 2, mods));
        let (term, rx) = fixture(b"\x1b[?1000h");
        assert!(term.mouse_button(0, 0, 2, false, mods));
        assert_eq!(input(&rx), b"\x1b[M;!!");
        assert!(!term.mouse_motion(0, 0, 2, mods));
        let (term, rx) = fixture(b"\x1b[?1003;1006h");
        assert!(term.mouse_motion(0, 0, 3, MouseModifiers::default()));
        assert_eq!(input(&rx), b"\x1b[<35;1;1M");
    }

    #[test]
    fn legacy_coordinate_limits_and_utf8() {
        assert_eq!(
            mouse_report(0, 500, 500, true, false, false),
            b"\x1b[M \xff\xff"
        );
        assert_eq!(
            mouse_report(0, 94, 95, true, false, true),
            b"\x1b[M \x7f\xc2\x80"
        );
        assert_eq!(
            mouse_report(2, u16::MAX, u16::MAX, false, true, false),
            b"\x1b[<2;65535;65535m"
        );
    }
}
