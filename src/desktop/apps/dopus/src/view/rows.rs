//! The [`FileList`] — one pane's sorted listing as a custom iced widget
//! (the `apps/term/src/cpu_widget.rs` pattern: a widget that draws its rows
//! itself).
//!
//! Layout is O(1): the widget knows its row height (shaped once per
//! font/size, the ced editor's `ensure_metrics` trick) and the scroll offset,
//! so it lays out nothing and draws only the rows in the viewport. Name,
//! size and modified paragraphs are shaped once per row and cached in the
//! widget's [`Tree`] state keyed by row path (a full re-shape every frame is
//! what ced's P0 round just paid down); a cache entry is re-shaped when the
//! text it was shaped from differs — which is also how the relative modified
//! times refresh on the 200 ms tick (app contract, law 1). Shaping happens
//! in `update` (which owns `&mut Tree`); `draw` only reads the cache, so a
//! row that scrolled in between update and draw waits one frame for its text
//! — icons draw immediately.
//!
//! Hit-testing is `y / row_h`; a click in a directory row's toggle zone
//! toggles its expansion, a click elsewhere selects the row, and a
//! double-click on a directory toggles too. The sort headers are composed
//! built-ins in `view` (they publish the `view.sort-*` actions), not part of
//! this widget.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use iced::advanced::image::{self as aimage, Renderer as _};
use iced::advanced::text::{Paragraph as _, self as atext};
use iced::advanced::widget::{Tree, tree};
use iced::advanced::{Clipboard, Layout, Renderer as _, Shell, Widget, layout, mouse, renderer};
use iced::{Element, Event, Length, Point, Rectangle, Size, alignment};
use iced_tiny_skia::Renderer;

use cosmix_dopus_core::VisibleRow;

use crate::icons::{self, Icons};
use crate::view::Look;

/// The shaped-paragraph type the tiny-skia renderer hands out.
type Para = <Renderer as atext::Renderer>::Paragraph;

/// Widget messages, mapped onto the app's by the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowsMsg {
    /// A row was clicked — select it.
    Select(PathBuf),
    /// A directory row's toggle zone (or a double-click) — expand/collapse.
    Toggle(PathBuf),
}

/// The icon size, logical px (rasterised at ×2 and drawn downscaled).
const ICON_PX: f32 = 16.0;
/// Vertical padding around each row's line.
const ROW_PAD: f32 = 3.0;
/// Pixels of indent per tree depth level.
const DEPTH_INDENT: f32 = 16.0;
/// Width of a directory row's toggle zone (the chevron), before the icon.
const TOGGLE_W: f32 = 16.0;
/// The secondary columns, right-aligned at the row's right edge (the sort
/// headers mirror these widths).
pub const SIZE_W: f32 = 90.0;
pub const MODIFIED_W: f32 = 150.0;
/// Column gap.
pub const GAP: f32 = 12.0;
/// List rows per wheel notch.
const WHEEL_ROWS: f32 = 3.0;
/// A second press on the same row inside this window is a double-click.
const DOUBLE_CLICK: Duration = Duration::from_millis(400);
/// Press and release within this distance is a click.
const CLICK_SLOP: f32 = 5.0;

/// One cached row: the shaped paragraphs and the text they were shaped from.
struct Cached {
    name: Para,
    name_of: String,
    size: Para,
    size_of: String,
    modified: Para,
    modified_of: String,
}

/// Tree state: the scroll offset, the shaped-row cache, the row height.
struct RowState {
    /// Pixels scrolled past the top of the list.
    offset: f32,
    row_h: f32,
    metrics_key: Option<(iced::Font, u32)>,
    /// The tint the cache was built for (a re-tint clears it).
    tint: String,
    last_selected: Option<PathBuf>,
    /// Where a button went down, waiting for its release.
    press: Option<(Point, usize)>,
    /// The last completed click: `(when, row)` — a second on the same row
    /// inside [`DOUBLE_CLICK`] is a double-click.
    last_click: Option<(Instant, usize)>,
    cache: HashMap<PathBuf, Cached>,
}

impl RowState {
    fn new() -> Self {
        Self {
            offset: 0.0,
            row_h: 22.0,
            metrics_key: None,
            tint: String::new(),
            last_selected: None,
            press: None,
            last_click: None,
            cache: HashMap::new(),
        }
    }

    fn clamp(&mut self, rows: usize, height: f32) {
        let max = (rows as f32 * self.row_h).saturating_sub(height).max(0.0);
        self.offset = self.offset.clamp(0.0, max);
    }

    /// The row index under list-space `y`, if any.
    fn row_at(&self, y: f32, rows: usize) -> Option<usize> {
        if y < 0.0 {
            return None;
        }
        let index = ((y + self.offset) / self.row_h) as usize;
        (index < rows).then_some(index)
    }

    fn is_visible(&self, index: usize, height: f32) -> bool {
        let top = index as f32 * self.row_h - self.offset;
        top + self.row_h >= 0.0 && top <= height
    }
}

/// One pane's listing.
pub struct FileList<'a> {
    rows: &'a [VisibleRow],
    selected: Option<&'a Path>,
    /// The directories currently expanded (chevron + children).
    expanded: &'a HashSet<PathBuf>,
    icons: &'a Icons,
    tint: &'a str,
    look: &'a Look<'a>,
}

impl<'a> FileList<'a> {
    pub fn new(
        rows: &'a [VisibleRow],
        selected: Option<&'a Path>,
        expanded: &'a HashSet<PathBuf>,
        icons: &'a Icons,
        tint: &'a str,
        look: &'a Look<'a>,
    ) -> Self {
        Self { rows, selected, expanded, icons, tint, look }
    }

    fn is_expanded(&self, path: &Path) -> bool {
        self.expanded.contains(path)
    }

    /// Row height from the theme's font metrics (ced's `ensure_metrics`
    /// trick): shape a sample line once per `(font, px)` and pad it.
    fn ensure_metrics(&self, st: &mut RowState) {
        let key = (self.look.ui_font, self.look.px.to_bits());
        if st.metrics_key == Some(key) {
            return;
        }
        let line_h = self.look.px * 1.4;
        let sample = Para::with_text(atext::Text {
            content: "Ag",
            bounds: Size::INFINITE,
            size: iced::Pixels(self.look.px),
            line_height: atext::LineHeight::Absolute(iced::Pixels(line_h)),
            font: self.look.ui_font,
            align_x: atext::Alignment::Left,
            align_y: alignment::Vertical::Top,
            shaping: atext::Shaping::Basic,
            wrapping: atext::Wrapping::None,
        });
        st.row_h = sample.min_bounds().height.max(line_h) + 2.0 * ROW_PAD;
        st.metrics_key = Some(key);
    }

    fn shape(content: &str, font: iced::Font, px: f32) -> Para {
        Para::with_text(atext::Text {
            content,
            bounds: Size::INFINITE,
            size: iced::Pixels(px),
            line_height: atext::LineHeight::Absolute(iced::Pixels(px * 1.4)),
            font,
            align_x: atext::Alignment::Left,
            align_y: alignment::Vertical::Top,
            shaping: atext::Shaping::Basic,
            wrapping: atext::Wrapping::None,
        })
    }

    /// Shape (or re-shape) a row's three columns. An entry whose source text
    /// differs — the relative modified time aged, a size landed — re-shapes.
    fn cache_row(&self, st: &mut RowState, row: &VisibleRow, now: SystemTime) {
        if st.tint != self.tint {
            // A re-tint means a new theme: fonts and colours may all differ.
            st.tint = self.tint.to_owned();
            st.cache.clear();
            st.metrics_key = None;
        }
        let size_text = if row.entry.is_dir {
            cosmix_dopus_core::format_child_count(row.entry.child_count)
        } else {
            row.entry.size.map(cosmix_dopus_core::format_size).unwrap_or_else(|| "—".into())
        };
        let modified_text =
            row.entry.modified.map(|m| cosmix_dopus_core::format_modified_at(m, now)).unwrap_or_else(|| "—".into());
        let name = row.entry.name.clone();
        if let Some(cached) = st.cache.get(&row.entry.path)
            && cached.name_of == name
            && cached.size_of == size_text
            && cached.modified_of == modified_text
        {
            return;
        }
        let shaped = Cached {
            name: Self::shape(&name, self.look.ui_font, self.look.px),
            name_of: name,
            size: Self::shape(&size_text, self.look.mono_font, self.look.mono_px),
            size_of: size_text,
            modified: Self::shape(&modified_text, self.look.mono_font, self.look.mono_px),
            modified_of: modified_text,
        };
        st.cache.insert(row.entry.path.clone(), shaped);
        // The cache only ever holds what viewports asked for; drop anything
        // the current listing no longer shows once it grows past a screenful
        // of headroom.
        if st.cache.len() > 2 * self.rows.len().max(64) {
            let keep: HashSet<PathBuf> = self.rows.iter().map(|r| r.entry.path.clone()).collect();
            st.cache.retain(|path, _| keep.contains(path));
        }
    }

    /// Follow the selection when it changes: keep the selected row visible.
    fn follow_selection(&self, st: &mut RowState, height: f32) {
        let Some(selected) = self.selected else { return };
        if st.last_selected.as_deref() == Some(selected) {
            return;
        }
        st.last_selected = Some(selected.to_path_buf());
        if let Some(index) = self.rows.iter().position(|row| row.entry.path.as_path() == selected) {
            let top = index as f32 * st.row_h;
            let bottom = top + st.row_h;
            if top < st.offset {
                st.offset = top;
            } else if bottom > st.offset + height {
                st.offset = bottom - height;
            }
        }
    }

    /// Shape every row the viewport will draw (called from `update`, which
    /// owns the `&mut Tree` the cache lives in).
    fn sync_cache(&self, st: &mut RowState, height: f32, now: SystemTime) {
        let first = (st.offset / st.row_h).floor().max(0.0) as usize;
        for (index, row) in self.rows.iter().enumerate().skip(first) {
            if index > first && !st.is_visible(index, height) {
                break;
            }
            self.cache_row(st, row, now);
        }
    }
}

impl Widget<crate::app::Msg, iced::Theme, Renderer> for FileList<'_> {
    fn size(&self) -> Size<Length> {
        Size::new(Length::Fill, Length::Fill)
    }

    fn state(&self) -> tree::State {
        tree::State::new(RowState::new())
    }

    fn layout(&mut self, _tree: &mut Tree, _renderer: &Renderer, limits: &layout::Limits) -> layout::Node {
        // O(1): rows hang off the scroll offset; nothing is laid out.
        layout::Node::new(limits.resolve(Length::Fill, Length::Fill, Size::ZERO))
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        _clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, crate::app::Msg>,
        viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();
        let Some(clip) = bounds.intersection(viewport) else { return };
        let st = tree.state.downcast_mut::<RowState>();
        self.ensure_metrics(st);
        self.follow_selection(st, clip.height);
        st.clamp(self.rows.len(), clip.height);
        self.sync_cache(st, clip.height, SystemTime::now());

        match event {
            Event::Mouse(mouse::Event::WheelScrolled { delta }) if cursor.is_over(clip) => {
                let lines = match delta {
                    mouse::ScrollDelta::Lines { y, .. } => *y * WHEEL_ROWS,
                    mouse::ScrollDelta::Pixels { y, .. } => *y / st.row_h,
                };
                if lines != 0.0 {
                    st.offset -= lines * st.row_h;
                    st.clamp(self.rows.len(), clip.height);
                    shell.capture_event();
                }
            }
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) if cursor.is_over(clip) => {
                let position = cursor.position().unwrap_or_default();
                if let Some(index) = st.row_at(position.y - bounds.y, self.rows.len()) {
                    st.press = Some((position, index));
                }
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) if cursor.is_over(clip) => {
                let Some((pressed_at, index)) = st.press.take() else { return };
                let position = cursor.position().unwrap_or_default();
                let moved = ((position.x - pressed_at.x).powi(2) + (position.y - pressed_at.y).powi(2)).sqrt() > CLICK_SLOP;
                if moved || index >= self.rows.len() {
                    return;
                }
                let row = &self.rows[index];
                // The toggle zone: the leftmost TOGGLE_W of the row, on a
                // directory — click it (or double-click the row) to expand.
                let row_x = position.x - bounds.x - row.depth as f32 * DEPTH_INDENT;
                let in_toggle = row.entry.is_dir && row_x < TOGGLE_W;
                let double =
                    st.last_click.is_some_and(|(when, at)| when.elapsed() < DOUBLE_CLICK && at == index);
                st.last_click = Some((Instant::now(), index));
                if in_toggle || (double && row.entry.is_dir) {
                    shell.publish(crate::app::Msg::Rows(RowsMsg::Toggle(row.entry.path.clone())));
                } else if !double {
                    shell.publish(crate::app::Msg::Rows(RowsMsg::Select(row.entry.path.clone())));
                }
                shell.capture_event();
            }
            _ => {}
        }
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        _theme: &iced::Theme,
        _style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        use iced::advanced::text::Renderer as _;
        let bounds = layout.bounds();
        let Some(clip) = bounds.intersection(viewport) else { return };
        let st = tree.state.downcast_ref::<RowState>();
        let t = self.look.tokens;

        // The selected row's full-width background, under everything.
        if let Some(selected) = self.selected
            && let Some(index) = self.rows.iter().position(|row| row.entry.path.as_path() == selected)
            && st.is_visible(index, clip.height)
        {
            let rect = Rectangle {
                x: bounds.x,
                y: bounds.y + index as f32 * st.row_h - st.offset,
                width: bounds.width,
                height: st.row_h,
            };
            if let Some(clipped) = rect.intersection(&clip) {
                renderer.fill_quad(renderer::Quad { bounds: clipped, ..renderer::Quad::default() }, t.selection);
            }
        }

        let first = (st.offset / st.row_h).floor().max(0.0) as usize;
        for (index, row) in self.rows.iter().enumerate().skip(first) {
            let y = bounds.y + index as f32 * st.row_h - st.offset;
            if y > clip.y + clip.height {
                break;
            }
            if y + st.row_h < clip.y {
                continue;
            }
            let baseline = y + ROW_PAD;
            let x = bounds.x + row.depth as f32 * DEPTH_INDENT;

            // Toggle chevron for directories; always the file icon.
            let icon_bounds = Rectangle {
                x: x + TOGGLE_W,
                y: baseline + (st.row_h - 2.0 * ROW_PAD - ICON_PX) / 2.0,
                width: ICON_PX,
                height: ICON_PX,
            };
            if row.entry.is_dir {
                let chevron =
                    if self.is_expanded(&row.entry.path) { icons::Icon::ChevronDown } else { icons::Icon::ChevronRight };
                if let Some(handle) = self.icons.get(chevron, self.tint, (ICON_PX as u32) * 2) {
                    renderer.draw_image(aimage::Image::new(handle), icon_bounds, clip);
                }
            }
            let file_icon = icons::file_icon(&row.entry.path, row.entry.is_dir, false);
            if let Some(handle) = self.icons.get(file_icon, self.tint, (ICON_PX as u32) * 2) {
                renderer.draw_image(aimage::Image::new(handle), icon_bounds, clip);
            }

            // Name; secondary columns right-aligned, in the mono role.
            let Some(cached) = st.cache.get(&row.entry.path) else { continue };
            renderer.fill_paragraph(&cached.name, Point::new(x + TOGGLE_W + ICON_PX + 6.0, baseline), t.text, clip);
            let modified_x = bounds.x + bounds.width - MODIFIED_W - 4.0;
            let size_x = modified_x - SIZE_W - GAP;
            renderer.fill_paragraph(&cached.size, Point::new(size_x, baseline), t.muted_text, clip);
            renderer.fill_paragraph(&cached.modified, Point::new(modified_x, baseline), t.muted_text, clip);
        }
    }

    fn mouse_interaction(
        &self,
        _tree: &Tree,
        _layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _viewport: &Rectangle,
        _renderer: &Renderer,
    ) -> mouse::Interaction {
        mouse::Interaction::Default
    }
}

impl<'a> From<FileList<'a>> for Element<'a, crate::app::Msg, iced::Theme, Renderer> {
    fn from(list: FileList<'a>) -> Self {
        Element::new(list)
    }
}
