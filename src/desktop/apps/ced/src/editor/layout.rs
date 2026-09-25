//! Editor geometry (plan §4.2): gutter, cell grid, row/column arithmetic,
//! scrollbars. Pure — no renderer — so it is unit-tested with plain metrics
//! (plan §7.1 "renderer metrics stub").
//!
//! The text grid starts exactly at `editor.x + gutter_w` and `editor.y`: row
//! `line` is drawn at `y + (line - first_line) · line_h`, column `c` at
//! `x + gutter_w + (c - x_cells) · cell_w` — the numbers `ced.layout` reports
//! and the nested gate checks.

use cosmix_edit_client::model::Scroll;
use iced::{Point, Rectangle, Size};

/// Width of the origin strip in the gutter (plan §4.5).
pub const STRIP_W: f32 = 4.0;
/// Overlay scrollbar thickness.
pub const SCROLLBAR_W: f32 = 10.0;
/// Smallest scrollbar thumb.
pub const MIN_THUMB: f32 = 24.0;
/// Lines kept between the caret and the top/bottom edge when following it.
pub const CARET_MARGIN_ROWS: usize = 2;
/// Cells kept between the caret and the left/right edge when following it.
pub const CARET_MARGIN_CELLS: usize = 4;

/// Font-derived sizes, logical px.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Metrics {
    pub cell_w: f32,
    pub line_h: f32,
}

/// Where everything sits for one frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Geometry {
    /// The whole widget.
    pub bounds: Rectangle,
    pub metrics: Metrics,
    /// Digits reserved for line numbers (0 when they are hidden).
    pub digits: usize,
    pub gutter_w: f32,
}

impl Geometry {
    pub fn new(bounds: Rectangle, metrics: Metrics, line_count: usize, line_numbers: bool) -> Self {
        let digits = if line_numbers { digits(line_count).max(3) } else { 0 };
        Self { bounds, metrics, digits, gutter_w: gutter_width(metrics, digits) }
    }

    /// The text area (right of the gutter).
    pub fn text_rect(&self) -> Rectangle {
        Rectangle { x: self.bounds.x + self.gutter_w, y: self.bounds.y, width: (self.bounds.width - self.gutter_w).max(0.0), height: self.bounds.height }
    }

    pub fn gutter_rect(&self) -> Rectangle {
        Rectangle { width: self.gutter_w.min(self.bounds.width), ..self.bounds }
    }

    /// Rows that fit entirely (what `ced.layout` calls `visible_rows`; Page
    /// Up/Down move by this).
    pub fn full_rows(&self) -> usize {
        ((self.bounds.height / self.metrics.line_h).floor() as usize).max(1)
    }

    /// Rows drawn, including a partial last one.
    pub fn drawn_rows(&self) -> usize {
        (self.bounds.height / self.metrics.line_h).ceil() as usize
    }

    /// Whole cells that fit in the text area.
    pub fn cols(&self) -> usize {
        ((self.text_rect().width / self.metrics.cell_w).floor() as usize).max(1)
    }

    pub fn row_y(&self, line: usize, scroll: Scroll) -> f32 {
        self.bounds.y + (line as f32 - scroll.first_line as f32) * self.metrics.line_h
    }

    pub fn cell_x(&self, cells: usize, scroll: Scroll) -> f32 {
        self.bounds.x + self.gutter_w + (cells as f32 - scroll.x_cells as f32) * self.metrics.cell_w
    }

    /// The (1-based line, fractional cell) under `p`, unclamped vertically
    /// (a drag above the view gives a line before `first_line`, saturating
    /// at 1). The caller clamps the line to the text.
    pub fn hit(&self, p: Point, scroll: Scroll) -> (usize, f32) {
        let row = ((p.y - self.bounds.y) / self.metrics.line_h).floor();
        let line = (scroll.first_line as f32 + row).max(1.0) as usize;
        let cells = ((p.x - self.bounds.x - self.gutter_w) / self.metrics.cell_w + scroll.x_cells as f32).max(0.0);
        (line, cells)
    }

    /// The caret rectangle at a visual position (2 px wide, one row high).
    pub fn caret_rect(&self, line: usize, cells: usize, scroll: Scroll) -> Rectangle {
        Rectangle { x: self.cell_x(cells, scroll), y: self.row_y(line, scroll), width: 2.0, height: self.metrics.line_h }
    }

    /// Vertical scrollbar track (overlay, right edge).
    pub fn vbar_track(&self) -> Rectangle {
        let t = self.text_rect();
        Rectangle { x: t.x + t.width - SCROLLBAR_W, y: t.y, width: SCROLLBAR_W, height: t.height }
    }

    /// Horizontal scrollbar track (overlay, bottom edge, left of the vbar).
    pub fn hbar_track(&self) -> Rectangle {
        let t = self.text_rect();
        Rectangle { x: t.x, y: t.y + t.height - SCROLLBAR_W, width: (t.width - SCROLLBAR_W).max(0.0), height: SCROLLBAR_W }
    }
}

/// Gutter: right-aligned line numbers plus a cell of padding, the origin
/// strip, a lint column one cell wide, and half a cell before the text.
pub fn gutter_width(m: Metrics, digits: usize) -> f32 {
    let numbers = if digits > 0 { (digits as f32 + 1.0) * m.cell_w } else { 0.0 };
    (numbers + STRIP_W + m.cell_w + m.cell_w * 0.5).round()
}

pub fn digits(n: usize) -> usize {
    n.max(1).ilog10() as usize + 1
}

/// A scrollbar thumb along a track: `(offset, length)` in px for a view of
/// `visible` units at `first` out of `total` units. `None` when everything
/// fits.
pub fn thumb(track_len: f32, total: usize, visible: usize, first: usize) -> Option<(f32, f32)> {
    if total <= visible || track_len <= 0.0 {
        return None;
    }
    let len = (track_len * visible as f32 / total as f32).clamp(MIN_THUMB.min(track_len), track_len);
    let range = (total - visible) as f32;
    let off = (track_len - len) * (first.min(total - visible) as f32 / range);
    Some((off, len))
}

/// The first unit shown when the thumb's leading edge is at `offset` px.
pub fn thumb_to_first(track_len: f32, total: usize, visible: usize, offset: f32) -> usize {
    let Some((_, len)) = thumb(track_len, total, visible, 0) else { return 0 };
    let free = (track_len - len).max(1.0);
    ((offset / free).clamp(0.0, 1.0) * (total - visible) as f32).round() as usize
}

/// Scroll so that `(line, cells)` is visible with margins; `rows`/`cols` are
/// the full rows/cells of the view. Returns the adjusted scroll.
pub fn follow(mut scroll: Scroll, line: usize, cells: usize, rows: usize, cols: usize, line_count: usize) -> Scroll {
    let margin_r = CARET_MARGIN_ROWS.min(rows.saturating_sub(1) / 2);
    if line < scroll.first_line + margin_r {
        scroll.first_line = line.saturating_sub(margin_r).max(1);
    } else if line + margin_r >= scroll.first_line + rows {
        scroll.first_line = (line + margin_r + 1).saturating_sub(rows).max(1);
    }
    scroll.first_line = scroll.first_line.min(line_count.max(1));
    let margin_c = CARET_MARGIN_CELLS.min(cols.saturating_sub(1) / 2);
    if cells < scroll.x_cells + margin_c {
        scroll.x_cells = cells.saturating_sub(margin_c);
    } else if cells + margin_c >= scroll.x_cells + cols {
        scroll.x_cells = (cells + margin_c + 1).saturating_sub(cols);
    }
    scroll
}

/// Clamp a scroll to the text: the last line may reach the top of the view.
pub fn clamp_scroll(mut scroll: Scroll, line_count: usize) -> Scroll {
    scroll.first_line = scroll.first_line.clamp(1, line_count.max(1));
    scroll
}

/// `bounds` size for the `LayoutReport` editor rect.
pub fn rect4(r: Rectangle) -> [f32; 4] {
    [r.x, r.y, r.width, r.height]
}

pub fn size_of(r: Rectangle) -> Size {
    Size::new(r.width, r.height)
}

#[cfg(test)]
mod tests {
    use super::*;

    const M: Metrics = Metrics { cell_w: 10.0, line_h: 20.0 };

    fn geo() -> Geometry {
        Geometry::new(Rectangle { x: 100.0, y: 50.0, width: 800.0, height: 405.0 }, M, 1234, true)
    }

    #[test]
    fn gutter_and_rows() {
        let g = geo();
        assert_eq!(g.digits, 4);
        assert_eq!(g.gutter_w, (5.0 * 10.0 + STRIP_W + 10.0 + 5.0_f32).round());
        assert_eq!(g.full_rows(), 20);
        assert_eq!(g.drawn_rows(), 21);
        assert_eq!(Geometry::new(g.bounds, M, 5, true).digits, 3, "at least three digits");
        let bare = Geometry::new(g.bounds, M, 5, false);
        assert_eq!(bare.gutter_w, (STRIP_W + 15.0_f32).round());
    }

    #[test]
    fn row_and_cell_positions_match_the_layout_report_rule() {
        let g = geo();
        let scroll = Scroll { first_line: 4, x_cells: 0 };
        // caret.y == editor.y + (10 − first_line)·line_height; caret.x == editor.x + gutter_w + 4·cell_w
        let caret = g.caret_rect(10, 4, scroll);
        assert_eq!(caret.y, 50.0 + 6.0 * 20.0);
        assert_eq!(caret.x, 100.0 + g.gutter_w + 40.0);
        let scrolled = Scroll { first_line: 4, x_cells: 3 };
        assert_eq!(g.cell_x(4, scrolled), 100.0 + g.gutter_w + 10.0);
    }

    #[test]
    fn hit_is_the_inverse_of_positioning() {
        let g = geo();
        let scroll = Scroll { first_line: 7, x_cells: 2 };
        for (line, cells) in [(7, 2), (9, 5), (20, 40)] {
            let p = Point::new(g.cell_x(cells, scroll) + 1.0, g.row_y(line, scroll) + 1.0);
            let (l, c) = g.hit(p, scroll);
            assert_eq!(l, line);
            assert_eq!(c.floor() as usize, cells);
        }
        let (l, c) = g.hit(Point::new(0.0, -500.0), scroll);
        assert_eq!((l, c), (1, 0.0), "above and left of the view saturate");
    }

    #[test]
    fn follow_keeps_the_caret_in_view_with_margins() {
        let s = Scroll { first_line: 10, x_cells: 0 };
        assert_eq!(follow(s, 15, 0, 20, 80, 1000), s, "inside: unchanged");
        assert_eq!(follow(s, 5, 0, 20, 80, 1000).first_line, 3);
        assert_eq!(follow(s, 40, 0, 20, 80, 1000).first_line, 23);
        assert_eq!(follow(s, 15, 100, 20, 80, 1000).x_cells, 25);
        let right = Scroll { first_line: 10, x_cells: 50 };
        assert_eq!(follow(right, 15, 10, 20, 80, 1000).x_cells, 6);
        assert_eq!(follow(s, 1, 0, 20, 80, 1000).first_line, 1);
    }

    #[test]
    fn thumbs() {
        assert_eq!(thumb(100.0, 10, 20, 0), None);
        let (off, len) = thumb(100.0, 100, 20, 0).unwrap();
        assert_eq!((off, len), (0.0, 24.0), "20% of the track, raised to the minimum");
        let (off, _) = thumb(100.0, 100, 20, 80).unwrap();
        assert_eq!(off, 76.0);
        assert_eq!(thumb_to_first(100.0, 100, 20, 76.0), 80);
        assert_eq!(thumb_to_first(100.0, 100, 20, 38.0), 40);
    }
}
