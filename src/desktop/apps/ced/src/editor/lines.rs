//! Line measurement for drawing and hit-testing (plan §4.2, D8, D9): walks
//! `view::clusters` over the visible window of one line, seeking long lines
//! through `view::line_checkpoints` (cached per text version) so a caret deep
//! in a 5 MiB line costs O(4 KiB), not O(line).

use std::collections::HashMap;
use std::ops::Range;

use cosmix_edit_client::model::{content_end, line_of};
use cosmix_edit_core::text::Text;
use cosmix_edit_core::view::{self, Cluster, MeasureCfg};

/// Lines longer than this seek through checkpoints.
pub const LONG_LINE: usize = 4096;

/// Checkpoints per line start, valid for one text version and measure.
#[derive(Default)]
pub struct Checkpoints {
    key: Option<(u64, usize, MeasureCfg)>,
    lines: HashMap<usize, Vec<(usize, usize)>>,
}

impl Checkpoints {
    /// The text changed identity when its commit count or length did.
    fn sync(&mut self, text: &Text, cfg: &MeasureCfg) {
        let key = Some((text.commit_calls(), text.len(), *cfg));
        if self.key != key {
            self.key = key;
            self.lines.clear();
        }
    }

    /// The best `(offset, cells)` start at or before `offset` / `cells` on
    /// `line` (the line start for short lines).
    fn start(&mut self, text: &Text, cfg: &MeasureCfg, line: usize, before: impl Fn(&(usize, usize)) -> bool) -> (usize, usize) {
        let Some(r) = text.line_range(line) else { return (text.len(), 0) };
        if r.len() <= LONG_LINE {
            return (r.start, 0);
        }
        self.sync(text, cfg);
        if self.lines.len() > 64 {
            self.lines.clear();
        }
        let ck = self.lines.entry(r.start).or_insert_with(|| view::line_checkpoints(text, cfg, line));
        ck.iter().rev().find(|c| before(c)).copied().unwrap_or((r.start, 0))
    }
}

/// One cluster placed on the grid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placed {
    pub range: Range<usize>,
    /// Starting cell (absolute, from the line start).
    pub cell: usize,
    pub cells: u8,
    pub is_tab: bool,
    pub ascii: bool,
}

/// The clusters of one line that intersect the cell window `[x0, x1)`.
#[derive(Debug, Clone, Default)]
pub struct LineCells {
    /// Line content range (no `\r\n` / `\n`).
    pub content: Range<usize>,
    pub placed: Vec<Placed>,
    /// Cells at the content end, when the walk reached it.
    pub end_cells: Option<usize>,
}

impl LineCells {
    /// Absolute cell of `offset` on this line, clipped to the walked window:
    /// before the window → its left edge, beyond it → `x1`.
    pub fn cell_of(&self, offset: usize, x1: usize) -> usize {
        if offset >= self.content.end {
            return self.end_cells.unwrap_or(x1);
        }
        // The first placed cluster ending after `offset` contains it or, when
        // `offset` lies left of the window, starts the window.
        self.placed.iter().find(|p| p.range.end > offset).map_or(self.end_cells.unwrap_or(x1), |p| p.cell)
    }
}

/// Walk `line`'s clusters covering cells `[x0, x1)`.
pub fn walk(text: &Text, cfg: &MeasureCfg, ck: &mut Checkpoints, line: usize, x0: usize, x1: usize) -> LineCells {
    let Some(r) = text.line_range(line) else { return LineCells::default() };
    let end = content_end(text, line);
    let (from, from_cells) = ck.start(text, cfg, line, |&(_, c)| c <= x0);
    let mut out = LineCells { content: r.start..end, placed: Vec::new(), end_cells: None };
    if from >= end {
        out.end_cells = Some(from_cells);
        return out;
    }
    let mut cell = from_cells;
    let mut reached_end = true;
    for Cluster { range, cells, is_tab, ascii } in view::clusters(text, cfg, from..end, from_cells) {
        if cell >= x1 {
            reached_end = false;
            break;
        }
        if range.start >= end {
            break;
        }
        let next = cell + cells as usize;
        if next > x0 || (cells == 0 && cell >= x0) {
            out.placed.push(Placed { range: range.clone(), cell, cells, is_tab, ascii });
        }
        cell = next;
    }
    if reached_end {
        out.end_cells = Some(cell);
    }
    out
}

/// The absolute cell of `offset` (exact, any line length).
pub fn cells_of(text: &Text, cfg: &MeasureCfg, ck: &mut Checkpoints, offset: usize) -> (usize, usize) {
    let line = line_of(text, offset);
    let end = content_end(text, line);
    let offset = offset.min(end);
    let (from, from_cells) = ck.start(text, cfg, line, |&(o, _)| o <= offset);
    let mut cell = from_cells;
    for c in view::clusters(text, cfg, from..end, from_cells) {
        if c.range.end > offset {
            break;
        }
        cell += c.cells as usize;
    }
    (line, cell)
}

/// The offset nearest to fractional cell `target` on `line` (mouse hits):
/// the boundary before a cluster when the target is in its first half.
pub fn offset_at(text: &Text, cfg: &MeasureCfg, ck: &mut Checkpoints, line: usize, target: f32) -> usize {
    let line = line.clamp(1, text.line_count().max(1));
    let end = content_end(text, line);
    let t = target.max(0.0) as usize;
    let (from, from_cells) = ck.start(text, cfg, line, |&(_, c)| c <= t);
    let mut cell = from_cells as f32;
    for c in view::clusters(text, cfg, from..end, from_cells) {
        if c.range.start >= end {
            break;
        }
        let w = c.cells as f32;
        if target < cell + w / 2.0 {
            return c.range.start;
        }
        cell += w;
    }
    end
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> MeasureCfg {
        MeasureCfg { tab_size: 4, ambiguous_wide: false }
    }

    #[test]
    fn tabs_and_wide_characters_on_the_grid() {
        let text = Text::from_text("a\tb中c\r\nnext").unwrap();
        let mut ck = Checkpoints::default();
        let lc = walk(&text, &cfg(), &mut ck, 1, 0, 80);
        let cells: Vec<(usize, u8)> = lc.placed.iter().map(|p| (p.cell, p.cells)).collect();
        assert_eq!(cells, [(0, 1), (1, 3), (4, 1), (5, 2), (7, 1)]);
        assert_eq!(lc.end_cells, Some(8), "the CR is not part of the content");
        assert_eq!(cells_of(&text, &cfg(), &mut ck, 3), (1, 5), "the wide char starts at cell 5");
        assert_eq!(cells_of(&text, &cfg(), &mut ck, 6), (1, 7));
        assert_eq!(cells_of(&text, &cfg(), &mut ck, 8), (1, 8), "inside the CRLF clamps to the content end");
        // Round trip offset ↔ cell across the tab and the wide char.
        for (o, _) in "a\tb中c".char_indices() {
            let (_, c) = cells_of(&text, &cfg(), &mut ck, o);
            assert_eq!(offset_at(&text, &cfg(), &mut ck, 1, c as f32), o, "offset {o} at cell {c}");
        }
        assert_eq!(offset_at(&text, &cfg(), &mut ck, 1, 6.2), 6, "past the middle of 中 → after it");
        assert_eq!(offset_at(&text, &cfg(), &mut ck, 1, 5.9), 3);
        assert_eq!(offset_at(&text, &cfg(), &mut ck, 1, 999.0), 7, "clamped before the CRLF");
    }

    #[test]
    fn a_window_clips_and_reports_offscreen_offsets_at_its_edges() {
        let s = "0123456789".repeat(3);
        let text = Text::from_text(&s).unwrap();
        let mut ck = Checkpoints::default();
        let lc = walk(&text, &cfg(), &mut ck, 1, 10, 20);
        assert_eq!(lc.placed.first().map(|p| p.cell), Some(10));
        assert_eq!(lc.placed.len(), 10);
        assert_eq!(lc.end_cells, None);
        assert_eq!(lc.cell_of(15, 20), 15);
        assert_eq!(lc.cell_of(25, 20), 20);
        assert_eq!(lc.cell_of(2, 20), 10, "left of the window → its left edge");
    }

    #[test]
    fn long_lines_seek_through_checkpoints() {
        let s = format!("{}\nend", "x".repeat(5 * 1024 * 1024));
        let text = Text::from_text(&s).unwrap();
        let mut ck = Checkpoints::default();
        let far = 5 * 1024 * 1024 - 3;
        assert_eq!(cells_of(&text, &cfg(), &mut ck, far), (1, far));
        let lc = walk(&text, &cfg(), &mut ck, 1, far, far + 100);
        assert_eq!(lc.placed.first().map(|p| p.range.start), Some(far));
        assert_eq!(lc.end_cells, Some(5 * 1024 * 1024));
        assert_eq!(offset_at(&text, &cfg(), &mut ck, 1, (far + 1) as f32), far + 1);
    }
}
