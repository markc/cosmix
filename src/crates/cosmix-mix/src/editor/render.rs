//! Pure reflow from a logical line, with no terminal reads, writes or modes.
//! Positions use zero-based cells and canonical eager wrapping: an exact fit
//! ends at column zero of the next row. The future terminal writer must resolve
//! the terminal's pending-wrap state to that position explicitly.
//!
//! Prompt escapes carry no width. Layout contains visible text only; a future
//! colour renderer must associate styles separately, never replay cursor-control
//! escapes. Buffer controls are displayed as caret notation, not executed.
use super::buffer::Buffer;
use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const MAX_LAYOUT_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Position {
    pub row: usize,
    pub column: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    Prompt,
    Buffer(Range<usize>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CellRun {
    pub position: Position,
    pub text: String,
    pub width: usize,
    pub source: Source,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    pub columns: usize,
    pub rows: usize,
    pub cursor: Position,
    pub end: Position,
    pub runs: Vec<CellRun>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutError {
    ZeroWidth,
    InvalidCursor,
    Limit,
}

/// Strip CSI, OSC (BEL or ST), DCS/SOS/PM/APC (ST), and ordinary ESC sequences.
/// Unterminated sequences are discarded to the end, never shown as text.
pub fn visible_prompt(prompt: &str) -> String {
    let mut out = String::with_capacity(prompt.len());
    let mut chars = prompt.chars().peekable();
    while let Some(ch) = chars.next() {
        let sequence = match ch {
            '\u{1b}' => chars.next(),
            '\u{9b}' => Some('['),
            '\u{9d}' => Some(']'),
            '\u{90}' => Some('P'),
            '\u{98}' => Some('X'),
            '\u{9e}' => Some('^'),
            '\u{9f}' => Some('_'),
            _ => {
                out.push(ch);
                continue;
            }
        };
        match sequence {
            Some('[') => {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            Some(kind @ (']' | 'P' | 'X' | '^' | '_')) => {
                while let Some(c) = chars.next() {
                    if c == '\u{9c}' || (kind == ']' && c == '\u{7}') {
                        break;
                    }
                    if c == '\u{1b}' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            Some(c) if (' '..='/').contains(&c) => {
                for c in chars.by_ref() {
                    if ('0'..='~').contains(&c) {
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// Intrinsic printable-cell width; line breaks/control characters add no width.
/// Tabs need a starting column, so their expansion belongs to `layout`.
pub fn prompt_width(prompt: &str) -> usize {
    UnicodeWidthStr::width(visible_prompt(prompt).as_str())
}

pub fn layout(prompt: &str, buffer: &Buffer, columns: usize) -> Result<Layout, LayoutError> {
    layout_text(prompt, buffer.text(), buffer.cursor(), columns)
}

/// Resize is simply another call with new columns; no mutable screen state.
pub fn layout_text(
    prompt: &str,
    text: &str,
    cursor: usize,
    columns: usize,
) -> Result<Layout, LayoutError> {
    if columns == 0 {
        return Err(LayoutError::ZeroWidth);
    }
    if prompt.len().saturating_add(text.len()) > MAX_LAYOUT_BYTES {
        return Err(LayoutError::Limit);
    }
    if cursor != text.len() && !text.grapheme_indices(true).any(|(i, _)| i == cursor) {
        return Err(LayoutError::InvalidCursor);
    }
    let mut result = Layout {
        columns,
        rows: 1,
        cursor: Position::default(),
        end: Position::default(),
        runs: Vec::new(),
    };
    for grapheme in visible_prompt(prompt).graphemes(true) {
        result.place(grapheme, Source::Prompt);
    }
    for (byte, grapheme) in text.grapheme_indices(true) {
        let before = result.place(grapheme, Source::Buffer(byte..byte + grapheme.len()));
        if byte == cursor {
            result.cursor = before;
        }
    }
    if cursor == text.len() {
        result.cursor = result.end;
    }
    result.rows = result.end.row + 1;
    Ok(result)
}

impl Layout {
    fn advance(&mut self, width: usize) {
        self.end.column += width;
        if self.end.column == self.columns {
            self.end.row += 1;
            self.end.column = 0;
        }
    }
    fn scalar_cells(&mut self, text: &str, source: Source) {
        for c in text.chars() {
            self.runs.push(CellRun {
                position: self.end,
                text: c.to_string(),
                width: 1,
                source: source.clone(),
            });
            self.advance(1);
        }
    }
    fn place(&mut self, grapheme: &str, source: Source) -> Position {
        let mut before = self.end;
        if matches!(grapheme, "\n" | "\r\n") {
            self.end.row += 1;
            self.end.column = 0;
        } else if grapheme == "\t" {
            let spaces = 8 - self.end.column % 8;
            self.scalar_cells(&" ".repeat(spaces), source);
        } else if grapheme.chars().any(char::is_control) {
            for ch in grapheme.chars() {
                let display = match ch {
                    '\u{0}'..='\u{1f}' => format!("^{}", char::from(ch as u8 + b'@')),
                    '\u{7f}' => "^?".to_owned(),
                    _ => "�".to_owned(),
                };
                self.scalar_cells(&display, source.clone());
            }
        } else {
            let width = UnicodeWidthStr::width(grapheme);
            // A double-width cluster cannot fit a one-column terminal. Render
            // a one-cell replacement explicitly, preserving its source range.
            let (display, width) = if width > self.columns {
                ("�", 1)
            } else {
                (grapheme, width)
            };
            if self.end.column + width > self.columns {
                self.end.row += 1;
                self.end.column = 0;
                before = self.end;
            }
            self.runs.push(CellRun {
                position: self.end,
                text: display.to_owned(),
                width,
                source,
            });
            self.advance(width);
        }
        before
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn p(row: usize, column: usize) -> Position {
        Position { row, column }
    }
    #[test]
    fn ansi_colour_osc_and_unterminated_sequences() {
        for prompt in [
            "\x1b[31m界> \x1b[0m",
            "\x1b]0;title\x07界> ",
            "\x1b]8;;https://example.org\x1b\\界\x1b]8;;\x1b\\> ",
        ] {
            assert_eq!(visible_prompt(prompt), "界> ");
            assert_eq!(prompt_width(prompt), 4);
        }
        assert_eq!(visible_prompt("a\x1b[31"), "a");
        assert_eq!(visible_prompt("a\x1bPignored\x1b\\b"), "ab");
        assert_eq!(visible_prompt("a\u{9b}31mb\u{9d}title\u{9c}c"), "abc");
    }
    #[test]
    fn empty_exact_fit_and_long_prompt() {
        let l = layout_text("", "", 0, 1).unwrap();
        assert_eq!(l.cursor, p(0, 0));
        assert_eq!(l.rows, 1);
        let l = layout_text("> ", "ab", 2, 4).unwrap();
        assert_eq!(l.end, p(1, 0));
        assert_eq!(l.rows, 2);
        let l = layout_text("abcdef", "x", 0, 2).unwrap();
        assert_eq!(l.cursor, p(3, 0));
        assert_eq!(l.end, p(3, 1));
        let l = layout_text("abc", "d", 1, 1).unwrap();
        assert_eq!(l.cursor, p(4, 0));
    }
    #[test]
    fn wide_wrap_and_cursor_before_cluster() {
        let l = layout_text("> ", "a界b", 1, 4).unwrap();
        assert_eq!(l.cursor, p(1, 0));
        assert_eq!(l.end, p(1, 3));
        assert_eq!(l.runs[3].text, "界");
        assert_eq!(l.runs[3].position, p(1, 0));
        let l = layout_text("", "界", 3, 1).unwrap();
        assert_eq!(l.runs[0].text, "�");
        assert_eq!(l.end, p(1, 0));
    }
    #[test]
    fn combining_and_family_are_indivisible() {
        for (s, width) in [("e\u{301}", 1), ("界", 2), ("👩‍👩‍👧‍👦", 2)] {
            let l = layout_text("", s, s.len(), 4).unwrap();
            assert_eq!(l.runs.len(), 1);
            assert_eq!(l.end, p(0, width));
            assert_eq!(l.runs[0].source, Source::Buffer(0..s.len()));
        }
        assert_eq!(
            layout_text("", "e\u{301}", 1, 4),
            Err(LayoutError::InvalidCursor)
        );
    }
    #[test]
    fn newline_tabs_and_controls_are_explicit() {
        let l = layout_text("x\n> ", "a\nb", 2, 5).unwrap();
        assert_eq!(l.cursor, p(2, 0));
        assert_eq!(l.end, p(2, 1));
        let l = layout_text("", "\t\x1b", 2, 10).unwrap();
        assert_eq!(l.end, p(1, 0));
        assert_eq!(
            l.runs.iter().map(|r| r.text.as_str()).collect::<String>(),
            "        ^["
        );
        let l = layout_text("", "a\n", 2, 1).unwrap();
        assert_eq!(l.end, p(2, 0));
    }
    #[test]
    fn resize_reflows_without_mutating_buffer() {
        let mut b = Buffer::default();
        b.insert("a界👩‍👩‍👧‍👦e\u{301}").unwrap();
        b.left().unwrap();
        let old = b.clone();
        let wide = layout("> ", &b, 20).unwrap();
        let narrow = layout("> ", &b, 4).unwrap();
        assert_eq!(wide.cursor, p(0, 7));
        assert_eq!(narrow.cursor, p(2, 0));
        assert_eq!(layout("> ", &b, 20).unwrap(), wide);
        assert_eq!(b, old);
    }
    #[test]
    fn every_boundary_at_small_widths_is_in_bounds() {
        let text = "e\u{301}界👩‍👩‍👧‍👦\nabc\t\x1b";
        for columns in 1..=12 {
            for cursor in text
                .grapheme_indices(true)
                .map(|(i, _)| i)
                .chain(std::iter::once(text.len()))
            {
                let l = layout_text("\x1b[32mlong prompt> \x1b[0m", text, cursor, columns).unwrap();
                assert!(l.cursor.column < columns);
                assert!(l.cursor.row < l.rows);
                for run in l.runs {
                    assert!(run.position.column + run.width <= columns);
                }
            }
        }
        assert_eq!(layout_text("", "", 0, 0), Err(LayoutError::ZeroWidth));
        assert_eq!(layout_text("", "", 1, 1), Err(LayoutError::InvalidCursor));
        assert_eq!(
            layout_text(&"x".repeat(MAX_LAYOUT_BYTES + 1), "", 0, 80),
            Err(LayoutError::Limit)
        );
    }
}
