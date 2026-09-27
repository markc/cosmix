//! The [`FileList`] — one pane's sorted listing as a custom iced widget
//! (the `apps/term/src/cpu_widget.rs` pattern: a widget that draws its rows
//! itself).
//!
//! Layout touches only visible icon tooltip regions; row height is shaped once per
//! font/size, the ced editor's `ensure_metrics` trick) and the scroll offset,
//! so it lays out hit regions and draws only the rows in the viewport. Name,
//! size and modified paragraphs are shaped once per row and cached in the
//! widget's [`Tree`] state keyed by row path (a full re-shape every frame is
//! what ced's P0 round just paid down); a cache entry is re-shaped when the
//! text it was shaped from differs. Absolute timestamps never age. Shaping happens
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
use std::time::{Duration, Instant};

use iced::advanced::image::{self as aimage, Renderer as _};
use iced::advanced::text::{self as atext, Paragraph as _};
use iced::advanced::widget::{Tree, tree};
use iced::advanced::{Clipboard, Layout, Renderer as _, Shell, Widget, layout, mouse, renderer};
use iced::{Element, Event, Length, Point, Rectangle, Size, alignment};
use iced_tiny_skia::Renderer;

use cosmix_dopus_core::{FileEntry, VisibleRow};

use crate::icons::{self, Icons};
use crate::view::Look;

/// The shaped-paragraph type the tiny-skia renderer hands out.
type Para = <Renderer as atext::Renderer>::Paragraph;

/// Widget messages, mapped onto the app's by the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowsMsg {
    /// Any press in the listing — the pane it belongs to becomes active.
    Press,
    /// A row was clicked — select it.
    Select(PathBuf),
    /// A directory row's toggle zone (or a double-click) — expand/collapse.
    Toggle(PathBuf),
}

/// One column contract for headers and rows. Secondary widths are measured
/// in the resolved mono role, including the widest absolute time form.
#[derive(Clone, Copy, Debug)]
pub struct Columns {
    pub name_min: f32,
    pub size: f32,
    pub modified: f32,
    pub gap: f32,
    pub pad: f32,
}
impl Columns {
    fn new(look: Look, rows: &[VisibleRow]) -> Self {
        let measure = |s: &str| -> f32 {
            FileList::shape(s, look.mono_font, look.small_px)
                .min_bounds()
                .width
        };
        Self {
            name_min: look.chrome.icon * 2.0 + look.chrome.small + Self::name_measure(look),
            size: listing_size_width(
                rows.iter().map(|row| size_text(&row.entry)),
                look.chrome.small,
                measure,
            ),
            modified: measure("88/88/88 88:88"),
            gap: look.chrome.gap,
            pad: look.chrome.pad,
        }
    }
    fn name_measure(look: Look) -> f32 {
        FileList::shape("MMMM", look.ui_font, look.px)
            .min_bounds()
            .width
    }
    /// Local x/width pairs shared by headers and rows. Preserve a usable
    /// name before secondary columns: hide Modified, then Size. Numeric
    /// values are never shortened into an ambiguous number.
    pub fn cells(self, width: f32) -> [(f32, f32); 3] {
        let pad = self.pad.min(width.max(0.0) / 2.0);
        let available = (width - 2.0 * pad).max(0.0);
        let modified = if available >= self.name_min + self.size + self.modified + 2.0 * self.gap {
            self.modified
        } else {
            0.0
        };
        let size_budget = available - self.name_min - self.gap;
        let size = if modified > 0.0 || size_budget >= self.size {
            self.size
        } else {
            0.0
        };
        let name = available
            - size
            - modified
            - if size > 0.0 { self.gap } else { 0.0 }
            - if modified > 0.0 { self.gap } else { 0.0 };
        let size_x = pad + name + if size > 0.0 { self.gap } else { 0.0 };
        [
            (pad, name),
            (size_x, size),
            (width.max(0.0) - pad - modified, modified),
        ]
    }

    /// Tree indentation yields to the same minimum name budget as columns.
    fn indentation(self, width: f32, depth: usize, icon: f32) -> f32 {
        (depth as f32 * icon).min((self.cells(width)[0].1 - self.name_min).max(0.0))
    }

    /// The actual text rectangle inside Name, in pane-local coordinates.
    /// Both shaping and drawing use this; decoration is subtracted once.
    fn name_text(self, width: f32, depth: usize, icon: f32, padding: f32) -> (f32, f32) {
        let (start, cell_width) = self.cells(width)[0];
        let decoration = self.indentation(width, depth, icon) + 2.0 * icon + padding;
        (start + decoration, (cell_width - decoration).max(0.0))
    }
}

fn size_text(entry: &FileEntry) -> String {
    if entry.is_dir {
        cosmix_dopus_core::format_child_count(entry.child_count)
    } else {
        entry
            .size
            .map(cosmix_dopus_core::format_size)
            .unwrap_or_else(|| "—".into())
    }
}

// Size/count strings use the mono role. Keep only a bounded set of longest
// strings; shaping every distinct file size makes large relists expensive.
const SIZE_CANDIDATES: usize = 4;

fn listing_size_width<S: AsRef<str>>(
    values: impl Iterator<Item = S>,
    padding: f32,
    measure: impl Fn(&str) -> f32,
) -> f32 {
    let floor = measure("99.9 MiB");
    let ceiling = measure("999999 items").max(floor);
    use unicode_segmentation::UnicodeSegmentation;
    let mut longest: Vec<(usize, S)> = Vec::with_capacity(SIZE_CANDIDATES);
    for value in values {
        let length = value.as_ref().graphemes(true).count();
        let position = longest.partition_point(|(n, _)| *n >= length);
        if position < SIZE_CANDIDATES {
            if longest.len() == SIZE_CANDIDATES {
                longest.pop();
            }
            longest.insert(position, (length, value));
        }
    }
    longest
        .iter()
        .map(|(_, s)| measure(s.as_ref()))
        .fold(floor, f32::max)
        .min(ceiling)
        + 2.0 * padding
}

type ColumnMetrics = (iced::Font, u32, iced::Font, u32, crate::theme::Chrome);

/// Shared header/row measurements. Unchanged signatures do no formatting or
/// shaping. Changed listings shape at most four Size candidates plus the
/// floor, ceiling, Modified sample and Name minimum (eight paragraphs).
#[derive(Default)]
pub struct ColumnCache {
    root: Option<PathBuf>,
    signature: Option<(u64, u64, usize)>,
    metrics: Option<ColumnMetrics>,
    columns: Option<Columns>,
}
impl ColumnCache {
    pub fn refresh(
        &mut self,
        look: Look,
        pane: &cosmix_dopus_core::PaneModel,
        rows: &[VisibleRow],
    ) {
        let metrics = (
            look.ui_font,
            look.px.to_bits(),
            look.mono_font,
            look.small_px.to_bits(),
            look.chrome,
        );
        if self.metrics.as_ref() != Some(&metrics) {
            self.signature = None;
        }
        self.refresh_columns(
            &pane.path,
            (pane.generation, pane.listing_revision, rows.len()),
            || Columns::new(look, rows),
        );
        self.metrics = Some(metrics);
    }

    fn refresh_columns(
        &mut self,
        root: &Path,
        signature: (u64, u64, usize),
        build: impl FnOnce() -> Columns,
    ) {
        let same_root = self.root.as_deref() == Some(root);
        if same_root && self.signature == Some(signature) {
            return;
        }
        let mut columns = build();
        if same_root && let Some(previous) = self.columns {
            columns.size = columns.size.max(previous.size);
        }
        self.columns = Some(columns);
        if !same_root {
            self.root = Some(root.to_path_buf());
        }
        self.signature = Some(signature);
    }

    pub fn get(&self, look: Look) -> Columns {
        self.columns.unwrap_or_else(|| Columns::new(look, &[]))
    }
}

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
    name_width: u32,
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
    metrics_key: Option<(iced::Font, u32, iced::Font, u32)>,
    /// The tint the cache was built for (a re-tint clears it).
    tint: String,
    last_selected: Option<PathBuf>,
    /// The listing the scroll state belongs to (the pane's root path): a new
    /// listing starts at the top.
    listing: Option<PathBuf>,
    /// Where a button went down, waiting for its release.
    press: Option<(Point, usize)>,
    /// The last completed click: `(when, row)` — a second on the same row
    /// inside [`DOUBLE_CLICK`] is a double-click.
    last_click: Option<(Instant, usize)>,
    cache: HashMap<PathBuf, Cached>,
}

impl RowState {
    fn new(look: Look) -> Self {
        Self {
            offset: 0.0,
            row_h: look.px.max(look.small_px) * 1.4 + 2.0 * look.chrome.small,
            metrics_key: None,
            tint: String::new(),
            last_selected: None,
            listing: None,
            press: None,
            last_click: None,
            cache: HashMap::new(),
        }
    }

    fn clamp(&mut self, rows: usize, height: f32) {
        let max = (rows as f32 * self.row_h - height).max(0.0);
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
    columns: Columns,
    rows: &'a [VisibleRow],
    selected: Option<&'a Path>,
    /// The pane's root path — the listing's identity (a change resets the
    /// scroll state).
    root: &'a Path,
    /// The directories currently expanded (chevron + children).
    expanded: &'a HashSet<PathBuf>,
    icons: &'a Icons,
    tint: &'a str,
    look: Look,
    tips: Vec<Element<'static, RowsMsg>>,
    open_label: String,
}

impl<'a> FileList<'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        rows: &'a [VisibleRow],
        selected: Option<&'a Path>,
        root: &'a Path,
        expanded: &'a HashSet<PathBuf>,
        icons: &'a Icons,
        tint: &'a str,
        look: Look,
        actions: &[crate::verbs::ActionRow],
        columns: Columns,
    ) -> Self {
        Self {
            columns,
            rows,
            selected,
            root,
            expanded,
            icons,
            tint,
            look,
            tips: Vec::new(),
            open_label: super::tips::action_label(
                actions,
                cosmix_actions::filemgr::FILE_OPEN,
                "Open",
            ),
        }
    }

    fn is_expanded(&self, path: &Path) -> bool {
        self.expanded.contains(path)
    }

    /// Row height from the theme's font metrics (ced's `ensure_metrics`
    /// trick): shape a sample line once per `(font, px)` and pad it.
    fn ensure_metrics(&self, st: &mut RowState) {
        let key = (
            self.look.ui_font,
            self.look.px.to_bits(),
            self.look.mono_font,
            self.look.small_px.to_bits(),
        );
        if st.metrics_key == Some(key) {
            return;
        }
        if st.metrics_key.is_some() {
            // Typography changed: every shaped row is stale (the cache keys
            // on text + tint, not on the font), so shape from scratch.
            st.cache.clear();
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
            shaping: atext::Shaping::Advanced,
            wrapping: atext::Wrapping::None,
        });
        st.row_h = sample
            .min_bounds()
            .height
            .max(line_h)
            .max(self.look.small_px * 1.4)
            + 2.0 * self.look.chrome.small;
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
            shaping: atext::Shaping::Advanced,
            wrapping: atext::Wrapping::None,
        })
    }

    /// Shape (or re-shape) a row's three columns. An entry whose source text
    /// differs — a relist or size landed — re-shapes.
    fn cache_row(&self, st: &mut RowState, row: &VisibleRow, width: f32) {
        if st.tint != self.tint {
            // A re-tint means a new theme: fonts and colours may all differ.
            st.tint = self.tint.to_owned();
            st.cache.clear();
            st.metrics_key = None;
        }
        let size_text = size_text(&row.entry);
        let modified_text = row
            .entry
            .modified
            .map(cosmix_dopus_core::format_modified_at)
            .unwrap_or_else(|| "—".into());
        let name = row.entry.name.clone();
        let (_, name_width) = self.columns.name_text(
            width,
            row.depth,
            self.look.chrome.icon,
            self.look.chrome.small,
        );
        if let Some(cached) = st.cache.get(&row.entry.path)
            && cached.name_of == name
            && cached.name_width == name_width.to_bits()
            && cached.size_of == size_text
            && cached.modified_of == modified_text
        {
            return;
        }
        let elided = super::elide::middle(&name, name_width, |s| {
            Self::shape(s, self.look.ui_font, self.look.px)
                .min_bounds()
                .width
        });
        let shaped = Cached {
            name: Self::shape(&elided, self.look.ui_font, self.look.px),
            name_of: name,
            name_width: name_width.to_bits(),
            size: Self::shape(&size_text, self.look.mono_font, self.look.small_px),
            size_of: size_text,
            modified: Self::shape(&modified_text, self.look.mono_font, self.look.small_px),
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

    /// A new listing (the pane navigated) resets the scroll state: the deep
    /// offset of the previous directory must not carry into the next one.
    fn reset_on_relist(&self, st: &mut RowState) {
        if st.listing.as_deref() == Some(self.root) {
            return;
        }
        st.listing = Some(self.root.to_path_buf());
        st.offset = 0.0;
        st.last_selected = None;
    }

    /// Follow the selection when it changes: keep the selected row visible.
    fn follow_selection(&self, st: &mut RowState, height: f32) {
        let Some(selected) = self.selected else {
            return;
        };
        if st.last_selected.as_deref() == Some(selected) {
            return;
        }
        st.last_selected = Some(selected.to_path_buf());
        if let Some(index) = self
            .rows
            .iter()
            .position(|row| row.entry.path.as_path() == selected)
        {
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
    fn sync_cache(&self, st: &mut RowState, height: f32, width: f32) {
        let first = (st.offset / st.row_h).floor().max(0.0) as usize;
        for (index, row) in self.rows.iter().enumerate().skip(first) {
            if index > first && !st.is_visible(index, height) {
                break;
            }
            self.cache_row(st, row, width);
        }
    }
}

impl Widget<RowsMsg, iced::Theme, Renderer> for FileList<'_> {
    fn diff(&self, _tree: &mut Tree) {
        // Preserve hover state until layout reconciles visible icon regions.
    }
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<RowState>()
    }
    fn size(&self) -> Size<Length> {
        Size::new(Length::Fill, Length::Fill)
    }

    fn state(&self) -> tree::State {
        tree::State::new(RowState::new(self.look))
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let size = limits.resolve(Length::Fill, Length::Fill, Size::ZERO);
        let st = tree.state.downcast_mut::<RowState>();
        self.ensure_metrics(st);
        self.reset_on_relist(st);
        self.follow_selection(st, size.height);
        st.clamp(self.rows.len(), size.height);
        let icon = self.look.chrome.icon;
        let cell = self.columns.cells(size.width)[0];
        let clip = Rectangle {
            x: cell.0,
            y: 0.0,
            width: cell.1,
            height: size.height,
        };
        let mut regions = Vec::new();
        let first = (st.offset / st.row_h).floor().max(0.0) as usize;
        for (index, row) in self.rows.iter().enumerate().skip(first) {
            let y = index as f32 * st.row_h - st.offset;
            if y >= size.height {
                break;
            }
            let x = cell.0 + self.columns.indentation(size.width, row.depth, icon);
            let y = y + (st.row_h - icon) / 2.0;
            if row.entry.is_dir {
                let label = format!(
                    "{} {}",
                    if self.is_expanded(&row.entry.path) {
                        "Collapse"
                    } else {
                        "Expand"
                    },
                    row.entry.name
                );
                if let Some(bounds) = (Rectangle {
                    x,
                    y,
                    width: icon,
                    height: icon,
                })
                .intersection(&clip)
                {
                    regions.push((bounds, label));
                }
            }
            if let Some(bounds) = (Rectangle {
                x: x + icon,
                y,
                width: icon,
                height: icon,
            })
            .intersection(&clip)
            {
                regions.push((
                    bounds,
                    format!(
                        "{}: {} — {}",
                        if row.entry.is_dir { "Folder" } else { "File" },
                        row.entry.name,
                        self.open_label
                    ),
                ));
            }
        }
        super::tips::regions(self.look, regions, &mut self.tips, tree, renderer, size)
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, RowsMsg>,
        viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();
        let Some(clip) = bounds.intersection(viewport) else {
            return;
        };
        let hover = if cursor.is_over(clip) {
            cursor
        } else {
            mouse::Cursor::Unavailable
        };
        for ((tip, state), child) in self
            .tips
            .iter_mut()
            .zip(&mut tree.children)
            .zip(layout.children())
        {
            tip.as_widget_mut().update(
                state, event, child, hover, renderer, clipboard, shell, &clip,
            );
        }
        let st = tree.state.downcast_mut::<RowState>();
        self.ensure_metrics(st);
        self.reset_on_relist(st);
        self.follow_selection(st, clip.height);
        st.clamp(self.rows.len(), clip.height);
        self.sync_cache(st, clip.height, bounds.width);

        match event {
            Event::Mouse(mouse::Event::WheelScrolled { delta }) if cursor.is_over(clip) => {
                let lines = match delta {
                    mouse::ScrollDelta::Lines { y, .. } => *y * WHEEL_ROWS,
                    mouse::ScrollDelta::Pixels { y, .. } => *y / st.row_h,
                };
                if lines != 0.0 {
                    st.offset -= lines * st.row_h;
                    st.clamp(self.rows.len(), clip.height);
                    shell.invalidate_layout();
                    shell.capture_event();
                }
            }
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))
                if cursor.is_over(clip) =>
            {
                // Any press in the listing activates the pane it belongs to
                // (the caller maps this onto `set_active_pane`), then the
                // press starts the row-click tracker.
                shell.publish(RowsMsg::Press);
                let position = cursor.position().unwrap_or_default();
                if let Some(index) = st.row_at(position.y - bounds.y, self.rows.len()) {
                    st.press = Some((position, index));
                }
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left))
                if cursor.is_over(clip) =>
            {
                let Some((pressed_at, index)) = st.press.take() else {
                    return;
                };
                let position = cursor.position().unwrap_or_default();
                let moved = ((position.x - pressed_at.x).powi(2)
                    + (position.y - pressed_at.y).powi(2))
                .sqrt()
                    > CLICK_SLOP;
                if moved || index >= self.rows.len() {
                    return;
                }
                let row = &self.rows[index];
                // The toggle zone: the leftmost TOGGLE_W of the row, on a
                // directory — click it to expand. A toggle is not a row
                // click: it stays out of the double-click tracker, so a fast
                // double-click in the chevron zone toggles once.
                let row_x = position.x
                    - bounds.x
                    - self.columns.cells(bounds.width)[0].0
                    - self
                        .columns
                        .indentation(bounds.width, row.depth, self.look.chrome.icon);
                let in_toggle = row.entry.is_dir && row_x >= 0.0 && row_x < self.look.chrome.icon;
                if in_toggle {
                    st.last_click = None;
                    shell.publish(RowsMsg::Toggle(row.entry.path.clone()));
                } else {
                    let double = st
                        .last_click
                        .is_some_and(|(when, at)| when.elapsed() < DOUBLE_CLICK && at == index);
                    st.last_click = Some((Instant::now(), index));
                    if double && row.entry.is_dir {
                        shell.publish(RowsMsg::Toggle(row.entry.path.clone()));
                    } else if !double {
                        shell.publish(RowsMsg::Select(row.entry.path.clone()));
                    }
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
        let Some(clip) = bounds.intersection(viewport) else {
            return;
        };
        let st = tree.state.downcast_ref::<RowState>();
        let t = self.look.tokens;
        let cells = self.columns.cells(bounds.width);
        let icon_px = self.look.chrome.icon;
        let row_pad = self.look.chrome.small;
        renderer.with_layer(clip, |renderer| {
            // The selected row's full-width background, under everything.
            if let Some(selected) = self.selected
                && let Some(index) = self
                    .rows
                    .iter()
                    .position(|row| row.entry.path.as_path() == selected)
                && st.is_visible(index, clip.height)
            {
                let rect = Rectangle {
                    x: bounds.x,
                    y: bounds.y + index as f32 * st.row_h - st.offset,
                    width: bounds.width,
                    height: st.row_h,
                };
                if let Some(clipped) = rect.intersection(&clip) {
                    renderer.fill_quad(
                        renderer::Quad {
                            bounds: clipped,
                            ..renderer::Quad::default()
                        },
                        t.selection,
                    );
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
                let baseline = y + row_pad;
                let x = bounds.x
                    + cells[0].0
                    + self.columns.indentation(bounds.width, row.depth, icon_px);
                let name_clip = Rectangle {
                    x: bounds.x + cells[0].0,
                    y: clip.y,
                    width: cells[0].1,
                    height: clip.height,
                };
                renderer.with_layer(name_clip, |renderer| {
                    // Directories paint the chevron in their toggle zone; every row
                    // paints its file icon (open when the directory is expanded).
                    if row.entry.is_dir {
                        let chevron_bounds = Rectangle {
                            x,
                            y: baseline + (st.row_h - 2.0 * row_pad - icon_px) / 2.0,
                            width: icon_px,
                            height: icon_px,
                        };
                        let chevron = if self.is_expanded(&row.entry.path) {
                            icons::Icon::ChevronDown
                        } else {
                            icons::Icon::ChevronRight
                        };
                        if let Some(handle) = self.icons.get(chevron, self.tint, icons::RASTER_PX) {
                            renderer.draw_image(aimage::Image::new(handle), chevron_bounds, clip);
                        }
                    }
                    let icon_bounds = Rectangle {
                        x: x + icon_px,
                        y: baseline + (st.row_h - 2.0 * row_pad - icon_px) / 2.0,
                        width: icon_px,
                        height: icon_px,
                    };
                    let expanded = self.is_expanded(&row.entry.path);
                    let file_icon = icons::file_icon(&row.entry.path, row.entry.is_dir, expanded);
                    if let Some(handle) = self.icons.get(file_icon, self.tint, icons::RASTER_PX) {
                        renderer.draw_image(aimage::Image::new(handle), icon_bounds, clip);
                    }

                    // Name; secondary columns right-aligned, in the mono role.
                    if let Some(cached) = st.cache.get(&row.entry.path) {
                        let color = if self.selected == Some(row.entry.path.as_path()) {
                            t.selection_text
                        } else {
                            t.text
                        };
                        renderer.fill_paragraph(
                            &cached.name,
                            Point::new(
                                bounds.x
                                    + self
                                        .columns
                                        .name_text(
                                            bounds.width,
                                            row.depth,
                                            icon_px,
                                            self.look.chrome.small,
                                        )
                                        .0,
                                baseline,
                            ),
                            color,
                            name_clip,
                        );
                    }
                });
                let Some(cached) = st.cache.get(&row.entry.path) else {
                    continue;
                };
                for (para, (start, width)) in
                    [(&cached.size, cells[1]), (&cached.modified, cells[2])]
                {
                    // Never show a clipped numeric prefix as a different value.
                    if width <= 0.0 || para.min_bounds().width > width {
                        continue;
                    }
                    let cell = Rectangle {
                        x: bounds.x + start,
                        y: clip.y,
                        width,
                        height: clip.height,
                    };
                    let color = if self.selected == Some(row.entry.path.as_path()) {
                        t.selection_text
                    } else {
                        t.muted_text
                    };
                    renderer.with_layer(cell, |renderer| {
                        renderer.fill_paragraph(
                            para,
                            Point::new(cell.x + width - para.min_bounds().width, baseline),
                            color,
                            cell,
                        )
                    });
                }
            }
        });
    }

    fn mouse_interaction(
        &self,
        _tree: &Tree,
        _layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _viewport: &Rectangle,
        _renderer: &Renderer,
    ) -> mouse::Interaction {
        mouse::Interaction::Idle
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: iced::Vector,
    ) -> Option<iced::advanced::overlay::Element<'b, RowsMsg, iced::Theme, Renderer>> {
        iced::advanced::overlay::from_children(
            &mut self.tips,
            tree,
            layout,
            renderer,
            viewport,
            translation,
        )
    }
}

impl<'a> From<FileList<'a>> for Element<'a, RowsMsg, iced::Theme, Renderer> {
    fn from(list: FileList<'a>) -> Self {
        Element::new(list)
    }
}

#[cfg(test)]
mod column_tests {
    use super::*;
    use cosmix_dopus_core::format_size;

    fn columns() -> Columns {
        Columns {
            name_min: 90.0,
            size: 90.0,
            modified: 180.0,
            gap: 12.0,
            pad: 8.0,
        }
    }

    #[test]
    fn five_thousand_values_have_bounded_shaping_and_unchanged_frames_do_no_work() {
        use std::cell::Cell;
        let calls = Cell::new(0);
        let measure = |s: &str| {
            calls.set(calls.get() + 1);
            FileList::shape(s, iced::Font::MONOSPACE, 11.0)
                .min_bounds()
                .width
        };
        let mut cache = ColumnCache::default();
        let root = Path::new("/listing");
        let signature = (1, 1, 5000);
        cache.refresh_columns(root, signature, || Columns {
            size: listing_size_width((0..5000).map(|i| format_size(i * 1031)), 4.0, measure),
            // The production constructor also shapes these two samples.
            name_min: measure("MMMM"),
            modified: measure("88/88/88 88:88"),
            ..columns()
        });
        assert_eq!(calls.get(), SIZE_CANDIDATES + 4);
        for _ in 0..100 {
            cache.refresh_columns(root, signature, || {
                panic!("unchanged listing was formatted")
            });
        }
        assert_eq!(calls.get(), SIZE_CANDIDATES + 4);
    }

    #[test]
    fn size_only_grows_until_the_pane_root_changes() {
        let mut cache = ColumnCache::default();
        let root = Path::new("/listing");
        let with_size = |size| Columns { size, ..columns() };
        cache.refresh_columns(root, (1, 1, 10), || with_size(80.0));
        // A count reply grows Size even though the row count is unchanged.
        cache.refresh_columns(root, (1, 2, 10), || with_size(110.0));
        assert_eq!(cache.columns.unwrap().size, 110.0);
        // Collapse, expand and a new generation (refresh/hidden toggle)
        // must not shrink it while the root identity stays the same.
        for signature in [(1, 2, 5), (1, 2, 10), (2, 3, 8)] {
            cache.refresh_columns(root, signature, || with_size(70.0));
            assert_eq!(cache.columns.unwrap().size, 110.0);
        }
        cache.refresh_columns(Path::new("/other"), (3, 4, 8), || with_size(70.0));
        assert_eq!(cache.columns.unwrap().size, 70.0);
    }

    #[test]
    fn small_file_listings_leave_most_width_for_names() {
        let measure = |s: &str| {
            FileList::shape(s, iced::Font::MONOSPACE, 11.0)
                .min_bounds()
                .width
        };
        let small = [format_size(12), format_size(512), format_size(2048)];
        let width = listing_size_width(small.iter().map(String::as_str), 4.0, measure);
        assert_eq!(width, measure("99.9 MiB") + 8.0);
        let crowded = listing_size_width(["999999 items"].into_iter(), 4.0, measure);
        assert!(width < crowded);
        let columns = Columns {
            size: width,
            modified: measure("88/88/88 88:88"),
            ..columns()
        };
        let cells = columns.cells(500.0);
        assert!(cells[0].1 > 250.0);
        assert_eq!(cells[1].1, width);
        assert!(cells[2].1 > 0.0);
        assert_eq!(
            listing_size_width(["999999999999 items"].into_iter(), 4.0, measure),
            crowded
        );
    }

    #[test]
    fn a_shaped_name_that_fits_the_actual_name_cell_is_not_elided() {
        let name = "ardour-session-Walthius_2009_Theme";
        let measure = |s: &str| {
            FileList::shape(s, iced::Font::DEFAULT, 14.0)
                .min_bounds()
                .width
        };
        let columns = columns();
        let (icon, padding, depth) = (16.0, 4.0, 2);
        assert!(measure(name) > 0.0);
        let decoration = (depth as f32 + 2.0) * icon + padding;
        let width = columns.pad * 2.0
            + columns.size
            + columns.modified
            + columns.gap * 2.0
            + decoration
            + measure(name)
            + 1.0;
        let (x, budget) = columns.name_text(width, depth, icon, padding);
        let cell = columns.cells(width)[0];
        assert!((x + budget - (cell.0 + cell.1)).abs() < 0.01);
        assert!(budget >= measure(name));
        assert_eq!(super::super::elide::middle(name, budget, measure), name);
    }

    #[test]
    fn header_and_rows_share_reserved_right_edges() {
        let columns = columns();
        for width in [400.0, 617.0, 920.0] {
            let [name, size, modified] = columns.cells(width);
            assert_eq!(name.0, columns.pad);
            assert_eq!(name.0 + name.1 + columns.gap, size.0);
            assert_eq!(size.1, columns.size);
            assert_eq!(size.0 + size.1 + columns.gap, modified.0);
            assert_eq!(modified.1, columns.modified);
            assert_eq!(modified.0 + modified.1 + columns.pad, width);
        }
    }

    #[test]
    fn capped_sidebars_preserve_names_without_eliding_numeric_size_at_190_pixels() {
        let columns = columns();
        let [name, size, modified] = columns.cells(190.0);
        assert!(name.1 >= columns.name_min);
        assert_eq!(size.1, 0.0);
        assert_eq!(modified.1, 0.0);
        assert_eq!(name.0 + name.1 + columns.pad, 190.0);
        // Deep tree rows keep the text budget; draw and hit-testing share this.
        assert_eq!(
            columns.indentation(190.0, 20, 16.0),
            name.1 - columns.name_min
        );
    }

    #[test]
    fn responsive_columns_hide_modified_before_size_and_never_overflow() {
        let columns = columns();
        assert_eq!(columns.cells(300.0)[1].1, columns.size);
        assert_eq!(columns.cells(300.0)[2].1, 0.0);
        assert_eq!(columns.cells(150.0)[1].1, 0.0);
        for width in 0..1000 {
            let width = width as f32;
            let cells = columns.cells(width);
            for (x, w) in cells {
                assert!(x >= 0.0 && w >= 0.0 && x + w <= width);
            }
            assert!(cells[0].1 >= columns.name_min.min((width - 2.0 * columns.pad).max(0.0)));
        }
    }
}
