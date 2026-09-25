//! Drawing (plan §4.2 draw order): background · current line · selection ·
//! remote selections · change tints · text runs · squiggles — then, in an
//! overlay layer, the IME preedit · remote carets · own caret · scrollbars ·
//! tooltips — and the gutter (numbers, origin strip, lint column).
//!
//! Text is logical-order on the cell grid (D8/D9): consecutive ASCII clusters
//! of one colour are one `fill_text`; every other cluster is drawn alone at
//! its cell with advanced shaping; a cluster over 4 KiB draws its first
//! 256 bytes (a visual cap only — `view` never splits clusters).

use std::ops::Range;
use std::time::Instant;

use cosmix_edit_client::diag::Severity;
use cosmix_edit_client::highlight::{HlClass, SliceBudget};
use cosmix_edit_client::model::line_of;
use cosmix_edit_core::origin::{Origin, OriginKind};
use iced::advanced::text::{self as atext};
use iced::advanced::{mouse, renderer};
use iced::{Border, Color, Font, Pixels, Point, Rectangle, Size};

use super::layout::{self as geo, Geometry, STRIP_W};
use super::lines::{self, LineCells};
use super::widget::{Editor, State, TINT};

/// Clusters longer than this draw only [`CLUSTER_DRAW_CAP`] bytes.
const HUGE_CLUSTER: usize = 4096;
const CLUSTER_DRAW_CAP: usize = 256;

struct Ctx<'e, 'a> {
    ed: &'e Editor<'a>,
    st: &'e State,
    g: &'e Geometry,
    now: Instant,
}

/// One visible row.
struct Row {
    line: usize,
    y: f32,
    cells: LineCells,
}

pub(super) fn draw<R: atext::Renderer<Font = Font>>(ed: &Editor<'_>, st: &State, g: &Geometry, r: &mut R, cursor: mouse::Cursor) {
    let c = Ctx { ed, st, g, now: Instant::now() };
    let p = ed.palette;
    let scroll = st.scroll;
    let text = ed.text;
    quad(r, g.bounds, p.background);

    let first = scroll.first_line;
    let last = (first + g.drawn_rows()).min(text.line_count());
    let (x0, x1) = (scroll.x_cells, scroll.x_cells + g.cols() + 1);
    let rows: Vec<Row> = {
        let mut ck = st.ck.borrow_mut();
        (first..=last.max(first))
            .filter(|&l| l <= text.line_count())
            .map(|line| Row { line, y: g.row_y(line, scroll), cells: lines::walk(text, &ed.view.measure, &mut ck, line, x0, x1) })
            .collect()
    };
    // The horizontal extent: the widest visible line, or beyond the window
    // when a visible line runs past it.
    let widest = rows.iter().map(|row| row.cells.end_cells.unwrap_or(x1 + g.cols())).max().unwrap_or(0);
    st.max_cells.set(widest);

    let trect = g.text_rect();
    r.with_layer(trect, |r| {
        c.line_backgrounds(r, &rows);
        c.text_runs(r, &rows);
        c.squiggles(r, &rows);
    });
    c.gutter(r, &rows, cursor);
    r.with_layer(g.bounds, |r| {
        c.preedit(r, &rows);
        c.remote_carets(r, &rows, cursor);
        c.own_caret(r, &rows);
        c.scrollbars(r, cursor);
    });
}

impl Ctx<'_, '_> {
    fn x(&self, cell: usize) -> f32 {
        self.g.cell_x(cell, self.st.scroll)
    }

    fn x_end(&self) -> usize {
        self.st.scroll.x_cells + self.g.cols() + 1
    }

    fn row_of(&self, rows: &[Row], offset: usize) -> Option<usize> {
        let line = line_of(self.ed.text, offset);
        rows.iter().position(|row| row.line == line)
    }

    /// Cells `[a, b)` of the part of `range` on `row`; a range continuing past
    /// the line end covers one extra half cell (the newline).
    fn span_on(&self, row: &Row, range: &Range<usize>) -> Option<(f32, f32)> {
        let content = &row.cells.content;
        let next = self.ed.text.line_start(row.line + 1).unwrap_or(usize::MAX);
        if range.is_empty() || range.start >= next || range.end <= content.start {
            return None;
        }
        let x1 = self.x_end();
        let a = row.cells.cell_of(range.start.max(content.start), x1);
        let b = row.cells.cell_of(range.end.min(content.end), x1);
        let mut right = self.x(b);
        if range.end > content.end && next != usize::MAX {
            right += self.g.metrics.cell_w * 0.5;
        }
        let left = self.x(a);
        (right > left).then_some((left, right))
    }

    fn row_rect(&self, row: &Row, left: f32, right: f32) -> Rectangle {
        Rectangle { x: left, y: row.y, width: right - left, height: self.g.metrics.line_h }
    }

    fn line_backgrounds<R: atext::Renderer<Font = Font>>(&self, r: &mut R, rows: &[Row]) {
        let ed = self.ed;
        let p = ed.palette;
        let t = self.g.text_rect();
        let sel = sel_range(ed.model.sel.anchor, ed.model.sel.head);
        if sel.is_empty()
            && let Some(i) = self.row_of(rows, sel.start)
        {
            quad(r, Rectangle { x: t.x, y: rows[i].y, width: t.width, height: self.g.metrics.line_h }, p.current_line);
        }
        for row in rows {
            if let Some((a, b)) = self.span_on(row, &sel) {
                quad(r, self.row_rect(row, a, b), p.selection);
            }
        }
        if ed.view.remote_carets {
            for (origin, sels) in &ed.model.remote {
                let colour = with_alpha(origin_colour(ed, origin), 0.18);
                for s in sels {
                    let range = sel_range(s.anchor, s.head);
                    if range.is_empty() {
                        continue;
                    }
                    for row in rows {
                        if let Some((a, b)) = self.span_on(row, &range) {
                            quad(r, self.row_rect(row, a, b), colour);
                        }
                    }
                }
            }
        }
        // Change tints: spans inserted by other origins, for TINT after first drawn.
        let mut seen = self.st.tint_seen.borrow_mut();
        let revs: Vec<u64> = ed.model.markers.changed.iter().map(|m| m.2).collect();
        seen.retain(|rev, _| revs.contains(rev));
        for (range, origin, rev) in &ed.model.markers.changed {
            let first = *seen.entry(*rev).or_insert(self.now);
            if range.is_empty() || self.now.duration_since(first) >= TINT {
                continue;
            }
            let colour = with_alpha(origin_colour(ed, origin), 0.22);
            for row in rows {
                if let Some((a, b)) = self.span_on(row, range) {
                    quad(r, self.row_rect(row, a, b), colour);
                }
            }
        }
    }

    fn text_runs<R: atext::Renderer<Font = Font>>(&self, r: &mut R, rows: &[Row]) {
        let ed = self.ed;
        let text = ed.text;
        let mut budget = SliceBudget::default();
        let mut buf = String::new();
        let clip = self.g.text_rect();
        for row in rows {
            let (Some(first), Some(last)) = (row.cells.placed.first(), row.cells.placed.last()) else { continue };
            let base = first.range.start;
            buf.clear();
            text.read(base..last.range.end, &mut buf);
            let spans = ed.highlight.with_spans(text, row.line, &mut budget, |s| s.map(<[_]>::to_vec)).unwrap_or_default();
            let mut si = 0;
            let mut run = Run::default();
            for pc in &row.cells.placed {
                while si < spans.len() && spans[si].0.end <= pc.range.start {
                    si += 1;
                }
                let class = spans.get(si).filter(|(sr, _)| sr.start <= pc.range.start).map_or(HlClass::Plain, |s| s.1);
                let slice = &buf[pc.range.start - base..pc.range.end - base];
                if pc.is_tab || (pc.ascii && pc.cells == 0) {
                    self.flush(r, &mut run, row, clip);
                    continue;
                }
                if pc.ascii {
                    if run.class != Some(class) || run.end_cell != pc.cell || !run.ascii {
                        self.flush(r, &mut run, row, clip);
                        run = Run { start_cell: pc.cell, end_cell: pc.cell, class: Some(class), ascii: true, text: String::new() };
                    }
                    run.text.push_str(slice);
                    run.end_cell = pc.cell + pc.cells as usize;
                } else {
                    self.flush(r, &mut run, row, clip);
                    let shown = if slice.len() > HUGE_CLUSTER { floor_boundary(slice, CLUSTER_DRAW_CAP) } else { slice };
                    run = Run { start_cell: pc.cell, end_cell: pc.cell + pc.cells as usize, class: Some(class), ascii: false, text: shown.to_string() };
                    self.flush(r, &mut run, row, clip);
                }
            }
            self.flush(r, &mut run, row, clip);
            if ed.view.whitespace {
                self.whitespace(r, row, clip);
            }
        }
    }

    fn flush<R: atext::Renderer<Font = Font>>(&self, r: &mut R, run: &mut Run, row: &Row, clip: Rectangle) {
        let Some(class) = run.class else { return };
        if run.text.is_empty() {
            *run = Run::default();
            return;
        }
        let p = self.ed.palette;
        let colour = if class == HlClass::Plain { p.text } else { p.hl(class) };
        let shaping = if run.ascii { atext::Shaping::Basic } else { atext::Shaping::Advanced };
        let width = (run.end_cell - run.start_cell + 1) as f32 * self.g.metrics.cell_w;
        self.text(r, std::mem::take(&mut run.text), Point::new(self.x(run.start_cell), row.y), width, colour, shaping, clip);
        *run = Run::default();
    }

    #[allow(clippy::too_many_arguments)]
    fn text<R: atext::Renderer<Font = Font>>(&self, r: &mut R, content: String, at: Point, width: f32, colour: Color, shaping: atext::Shaping, clip: Rectangle) {
        let v = &self.ed.view;
        r.fill_text(
            atext::Text {
                content,
                bounds: Size::new(width, self.g.metrics.line_h),
                size: Pixels(v.px),
                line_height: atext::LineHeight::Absolute(Pixels(self.g.metrics.line_h)),
                font: v.font,
                align_x: atext::Alignment::Left,
                align_y: iced::alignment::Vertical::Top,
                shaping,
                wrapping: atext::Wrapping::None,
            },
            at,
            colour,
            clip,
        );
    }

    /// Show whitespace: `·` per space, `→` per tab, one text call per row.
    fn whitespace<R: atext::Renderer<Font = Font>>(&self, r: &mut R, row: &Row, clip: Rectangle) {
        let Some(first) = row.cells.placed.first() else { return };
        let mut s = String::new();
        let mut any = false;
        let mut buf = String::new();
        for pc in &row.cells.placed {
            let blank = |n: usize, s: &mut String| s.extend(std::iter::repeat_n(' ', n));
            if pc.is_tab {
                s.push('→');
                blank((pc.cells as usize).saturating_sub(1), &mut s);
                any = true;
            } else if pc.ascii && pc.range.len() == 1 {
                buf.clear();
                self.ed.text.read(pc.range.clone(), &mut buf);
                if buf == " " {
                    s.push('·');
                    any = true;
                } else {
                    blank(pc.cells as usize, &mut s);
                }
            } else {
                blank(pc.cells as usize, &mut s);
            }
        }
        if any {
            let width = (s.chars().count() + 1) as f32 * self.g.metrics.cell_w;
            self.text(r, s, Point::new(self.x(first.cell), row.y), width, self.ed.palette.gutter_text, atext::Shaping::Advanced, clip);
        }
    }

    fn squiggles<R: atext::Renderer<Font = Font>>(&self, r: &mut R, rows: &[Row]) {
        let p = self.ed.palette;
        let cw = self.g.metrics.cell_w;
        for d in self.ed.diagnostics.items() {
            let Some(i) = self.row_of(rows, d.range.start) else { continue };
            let row = &rows[i];
            let x1 = self.x_end();
            let a = self.x(row.cells.cell_of(d.range.start, x1));
            let b = self.x(row.cells.cell_of(d.range.end.min(row.cells.content.end), x1)).max(a + cw);
            let base = row.y + self.g.metrics.line_h - 3.0;
            let (colour, dotted) = match d.severity {
                Severity::Error => (p.error, false),
                Severity::Warning => (p.warning, false),
                Severity::Note => (p.note, true),
            };
            let mut x = a;
            let mut up = false;
            while x < b {
                let w = (b - x).min(2.0);
                if dotted {
                    if !up {
                        quad(r, Rectangle { x, y: base + 1.0, width: w.min(1.5), height: 1.5 }, colour);
                    }
                } else {
                    quad(r, Rectangle { x, y: base + if up { 0.0 } else { 1.5 }, width: w, height: 1.5 }, colour);
                }
                up = !up;
                x += 2.0;
            }
        }
    }

    fn gutter<R: atext::Renderer<Font = Font>>(&self, r: &mut R, rows: &[Row], cursor: mouse::Cursor) {
        let ed = self.ed;
        let p = ed.palette;
        let g = self.g;
        let gr = g.gutter_rect();
        quad(r, gr, p.gutter_background);
        let cw = g.metrics.cell_w;
        let caret_line = line_of(ed.text, ed.model.sel.head);
        if g.digits > 0 {
            for row in rows {
                let n = row.line.to_string();
                let x = gr.x + (g.digits - n.len().min(g.digits)) as f32 * cw + cw * 0.5;
                let colour = if row.line == caret_line { p.text } else { p.gutter_text };
                self.text(r, n, Point::new(x, row.y), (g.digits + 1) as f32 * cw, colour, atext::Shaping::Basic, gr);
            }
        }
        // Origin strip: lines other origins changed since the tab was focused.
        let strip_x = gr.x + if g.digits > 0 { (g.digits as f32 + 1.0) * cw } else { 0.0 };
        let (Some(top), Some(bottom)) = (rows.first(), rows.last()) else { return };
        let mut hover: Option<(&Origin, u64, f32)> = None;
        for (range, origin, rev) in &ed.model.markers.changed {
            let l0 = line_of(ed.text, range.start).max(top.line);
            let l1 = line_of(ed.text, range.end).min(bottom.line);
            if l0 > l1 {
                continue;
            }
            let y0 = g.row_y(l0, self.st.scroll);
            let y1 = g.row_y(l1 + 1, self.st.scroll);
            let bar = Rectangle { x: strip_x, y: y0, width: STRIP_W, height: y1 - y0 };
            quad(r, bar, origin_colour(ed, origin));
            if let Some(pos) = cursor.position()
                && pos.y >= y0
                && pos.y < y1
                && pos.x >= strip_x - 2.0
                && pos.x < strip_x + STRIP_W + cw
            {
                hover = Some((origin, *rev, pos.y));
            }
        }
        // Lint column: one dot per line, worst severity wins.
        let lint_x = strip_x + STRIP_W + cw * 0.25;
        let mut worst: Vec<(usize, Severity)> = Vec::new();
        for d in ed.diagnostics.items() {
            let line = line_of(ed.text, d.range.start);
            if line < top.line || line > bottom.line {
                continue;
            }
            match worst.iter_mut().find(|(l, _)| *l == line) {
                Some((_, s)) if rank(d.severity) > rank(*s) => *s = d.severity,
                Some(_) => {}
                None => worst.push((line, d.severity)),
            }
        }
        for (line, sev) in worst {
            let colour = match sev {
                Severity::Error => p.error,
                Severity::Warning => p.warning,
                Severity::Note => p.note,
            };
            let d = (cw * 0.5).min(g.metrics.line_h * 0.4);
            let y = g.row_y(line, self.st.scroll) + (g.metrics.line_h - d) / 2.0;
            let dot = renderer::Quad { bounds: Rectangle { x: lint_x, y, width: d, height: d }, border: Border { radius: (d / 2.0).into(), ..Border::default() }, ..renderer::Quad::default() };
            r.fill_quad(dot, colour);
        }
        if let Some((origin, rev, y)) = hover {
            let age = self.st.tint_seen.borrow().get(&rev).map(|t| self.now.duration_since(*t).as_secs());
            let when = age.map_or(String::new(), |s| format!(" · {}", ago(s)));
            let label = format!("{origin} · rev {rev}{when}");
            r.with_layer(g.bounds, |r| self.chip(r, &label, Point::new(strip_x + STRIP_W + 4.0, y + 12.0), origin_colour(ed, origin)));
        }
    }

    /// A small label: background, accent edge, text.
    fn chip<R: atext::Renderer<Font = Font>>(&self, r: &mut R, label: &str, at: Point, accent: Color) {
        let p = self.ed.palette;
        let cw = self.g.metrics.cell_w;
        let w = (label.chars().count() as f32 + 1.0) * cw;
        let h = self.g.metrics.line_h;
        let x = at.x.min(self.g.bounds.x + self.g.bounds.width - w - 2.0).max(self.g.bounds.x);
        let rect = Rectangle { x, y: at.y, width: w, height: h };
        let bg = renderer::Quad { bounds: rect, border: Border { radius: 4.0.into(), width: 1.0, color: accent }, ..renderer::Quad::default() };
        r.fill_quad(bg, p.gutter_background);
        self.text(r, label.to_string(), Point::new(x + cw * 0.5, at.y), w, p.text, atext::Shaping::Advanced, self.g.bounds);
    }

    fn preedit<R: atext::Renderer<Font = Font>>(&self, r: &mut R, rows: &[Row]) {
        let st = self.st;
        if !st.ime.active() {
            return;
        }
        let caret = self.ed.caret_rect(st, self.g);
        if !rows.iter().any(|row| row.y == caret.y) {
            return;
        }
        let cells: usize = st.ime.preedit.chars().map(|c| if c.is_ascii() { 1 } else { 2 }).sum();
        let w = cells as f32 * self.g.metrics.cell_w;
        let p = self.ed.palette;
        quad(r, Rectangle { x: caret.x, y: caret.y, width: w, height: caret.height }, p.background);
        self.text(r, st.ime.preedit.clone(), Point::new(caret.x, caret.y), w + self.g.metrics.cell_w, p.text, atext::Shaping::Advanced, self.g.text_rect());
        quad(r, Rectangle { x: caret.x, y: caret.y + caret.height - 2.0, width: w, height: 1.0 }, p.caret);
    }

    fn remote_carets<R: atext::Renderer<Font = Font>>(&self, r: &mut R, rows: &[Row], cursor: mouse::Cursor) {
        let ed = self.ed;
        if !ed.view.remote_carets {
            return;
        }
        let t = self.g.text_rect();
        for (origin, sels) in &ed.model.remote {
            let colour = origin_colour(ed, origin);
            for s in sels {
                let Some(i) = self.row_of(rows, s.head) else { continue };
                let row = &rows[i];
                let x = self.x(row.cells.cell_of(s.head, self.x_end()));
                if x < t.x || x > t.x + t.width {
                    continue;
                }
                quad(r, Rectangle { x, y: row.y, width: 2.0, height: self.g.metrics.line_h }, colour);
                quad(r, Rectangle { x: x - 2.0, y: row.y, width: 6.0, height: 3.0 }, colour);
                let hovered = cursor.position().is_some_and(|p| p.y >= row.y && p.y < row.y + self.g.metrics.line_h && t.contains(p));
                if hovered {
                    let above = if row.y - self.g.metrics.line_h >= t.y { row.y - self.g.metrics.line_h } else { row.y + self.g.metrics.line_h };
                    self.chip(r, &origin.to_string(), Point::new(x, above), colour);
                }
            }
        }
    }

    fn own_caret<R: atext::Renderer<Font = Font>>(&self, r: &mut R, rows: &[Row]) {
        let ed = self.ed;
        let st = self.st;
        if st.ime.active() {
            return;
        }
        let Some(i) = self.row_of(rows, ed.model.sel.head) else { return };
        let row = &rows[i];
        let t = self.g.text_rect();
        let cell = row.cells.cell_of(ed.model.sel.head, self.x_end());
        let x = self.x(cell);
        if x < t.x - 1.0 || x > t.x + t.width {
            return;
        }
        let p = ed.palette;
        let h = self.g.metrics.line_h;
        if !ed.view.focused {
            quad(r, Rectangle { x, y: row.y, width: 1.0, height: h }, with_alpha(p.caret, 0.5));
        } else if ed.model.overwrite {
            quad(r, Rectangle { x, y: row.y, width: self.g.metrics.cell_w, height: h }, with_alpha(p.caret, 0.45));
        } else {
            quad(r, Rectangle { x, y: row.y, width: 2.0, height: h }, p.caret);
        }
    }

    fn scrollbars<R: atext::Renderer<Font = Font>>(&self, r: &mut R, cursor: mouse::Cursor) {
        let g = self.g;
        let st = self.st;
        let rows = g.full_rows();
        let colour = self.ed.palette.gutter_text;
        let v = g.vbar_track();
        if let Some((off, len)) = geo::thumb(v.height, self.ed.line_count() + rows - 1, rows, st.scroll.first_line - 1) {
            let hot = cursor.is_over(v);
            let rect = Rectangle { x: v.x + 3.0, y: v.y + off, width: v.width - 5.0, height: len };
            rounded(r, rect, with_alpha(colour, if hot { 0.6 } else { 0.3 }));
        }
        let h = g.hbar_track();
        let cols = g.cols();
        let total = st.max_cells.get().max(st.scroll.x_cells + cols);
        if let Some((off, len)) = geo::thumb(h.width, total, cols, st.scroll.x_cells) {
            let hot = cursor.is_over(h);
            let rect = Rectangle { x: h.x + off, y: h.y + 3.0, width: len, height: h.height - 5.0 };
            rounded(r, rect, with_alpha(colour, if hot { 0.6 } else { 0.3 }));
        }
    }
}


#[derive(Default)]
struct Run {
    start_cell: usize,
    end_cell: usize,
    class: Option<HlClass>,
    ascii: bool,
    text: String,
}

fn quad<R: iced::advanced::Renderer>(r: &mut R, bounds: Rectangle, colour: Color) {
    if colour.a <= 0.0 || bounds.width <= 0.0 || bounds.height <= 0.0 {
        return;
    }
    r.fill_quad(renderer::Quad { bounds, ..renderer::Quad::default() }, colour);
}

fn rounded<R: iced::advanced::Renderer>(r: &mut R, bounds: Rectangle, colour: Color) {
    let radius = (bounds.width.min(bounds.height) / 2.0).into();
    r.fill_quad(renderer::Quad { bounds, border: Border { radius, ..Border::default() }, ..renderer::Quad::default() }, colour);
}

fn with_alpha(c: Color, a: f32) -> Color {
    Color { a: c.a * a, ..c }
}

fn origin_colour(ed: &Editor<'_>, origin: &Origin) -> Color {
    if origin.kind == OriginKind::Agent { ed.palette.agent } else { ed.palette.human_other }
}

fn sel_range(a: usize, b: usize) -> Range<usize> {
    a.min(b)..a.max(b)
}

fn rank(s: Severity) -> u8 {
    match s {
        Severity::Note => 0,
        Severity::Warning => 1,
        Severity::Error => 2,
    }
}

fn floor_boundary(s: &str, max: usize) -> &str {
    let mut end = max.min(s.len());
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn ago(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s ago"),
        60..=3599 => format!("{}m ago", secs / 60),
        _ => format!("{}h ago", secs / 3600),
    }
}

