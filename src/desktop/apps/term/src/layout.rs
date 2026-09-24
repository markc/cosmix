//! Where every pane goes, as plain arithmetic.
//!
//! The pane tree is the core's (`cosmix_term_core::panes`), and its split
//! ratios are the only layout truth — the Bus's `term.pane.split` and a
//! keyboard split produce the same tree. This module turns that tree into
//! rectangles for the window, and the rectangles into grid sizes for the PTYs.
//!
//! Two things it adds over the core's own `PaneTree::leaves`, both because
//! the grid is a nearest-sampled texture laid out 1:1 on physical pixels:
//!
//! - **Every split edge lands on a whole physical pixel.** A pane starting at
//!   x = 450.2 physical would put the texture half a pixel off the device
//!   grid, and a nearest sampler then doubles or drops a column of glyph
//!   pixels. The core's ratios are kept; only the edge is snapped.
//! - **Each pane gets a border of a whole number of physical pixels** on all
//!   sides, focused or not — only its colour marks focus. The Bevy frontend
//!   draws a border only on the focused pane, which changes the interior and
//!   so reflows the PTY every time focus moves; this does not.
//!
//! `view` builds its widget tree from [`split`] and the grid sizes from
//! [`panes`], which calls the same function — so the two cannot disagree.

use cosmix_term_core::panes::{Geometry, PaneTree, SplitDir};
use cosmix_term_core::tabs::TabSet;

/// Same clamps as the Bevy frontend, and for the same reason: a grid wider
/// than 4096 physical pixels exceeds the texture size every GPU is guaranteed
/// to support, and a PTY is not obliged to cope with 10,000 columns.
pub const MAX_COLS: u16 = 240;
pub const MAX_ROWS: u16 = 100;
pub const MAX_TEXTURE: u32 = 4096;

/// Logical height of the tab strip. Fixed so the pane area is known before
/// iced lays anything out — the grid size has to reach the PTY from `update`.
pub const STRIP_HEIGHT: f32 = 30.0;

/// The active tab's pane tree with the terminals taken out: what `view`
/// needs, without holding the `TabSet` lock while it builds widgets.
#[derive(Clone, Debug, PartialEq)]
pub enum Node {
    Leaf(u64),
    Split {
        dir: SplitDir,
        ratio: f32,
        first: Box<Node>,
        second: Box<Node>,
    },
}

impl Node {
    pub fn of(tree: &PaneTree) -> Self {
        match tree {
            PaneTree::Leaf(pane) => Self::Leaf(pane.id),
            PaneTree::Split {
                dir,
                ratio,
                first,
                second,
            } => Self::Split {
                dir: *dir,
                ratio: *ratio,
                first: Box::new(Self::of(first)),
                second: Box::new(Self::of(second)),
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct TabLabel {
    pub id: u64,
    pub title: String,
    pub active: bool,
}

/// Everything `view` draws that is not pixels.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Shape {
    pub tabs: Vec<TabLabel>,
    /// `None` only when the tab set is empty, i.e. while the app is exiting.
    pub tree: Option<Node>,
    pub active_pane: u64,
}

impl Shape {
    pub fn of(tabs: &TabSet) -> Self {
        if tabs.is_empty() {
            return Self::default();
        }
        let active = tabs.active_tab();
        Self {
            tabs: tabs
                .list()
                .into_iter()
                .map(|tab| TabLabel {
                    id: tab.id,
                    title: tab.title,
                    active: tab.active,
                })
                .collect(),
            tree: Some(Node::of(&active.tree)),
            active_pane: active.active_pane,
        }
    }

    /// Pane ids on screen, in tree order.
    pub fn visible(&self) -> Vec<u64> {
        let mut out = Vec::new();
        if let Some(tree) = &self.tree {
            collect_ids(tree, &mut out);
        }
        out
    }
}

fn collect_ids(node: &Node, out: &mut Vec<u64>) {
    match node {
        Node::Leaf(id) => out.push(*id),
        Node::Split { first, second, .. } => {
            collect_ids(first, out);
            collect_ids(second, out);
        }
    }
}

/// `value` logical px, moved to the nearest whole physical pixel.
fn snap(value: f32, scale: f32) -> f32 {
    (value * scale).round() / scale
}

/// A pane border: one logical pixel, rounded to whole physical pixels and
/// never zero.
pub fn border(scale: f32) -> f32 {
    scale.round().max(1.0) / scale
}

/// The tab strip's height on this output: [`STRIP_HEIGHT`] on a whole
/// physical pixel, so the panes below it start on one. `view` sizes the strip
/// with this and [`content`] starts below it — one number, not two.
pub fn strip_height(scale: f32) -> f32 {
    snap(STRIP_HEIGHT, scale)
}

/// The area below the tab strip, for a window of `width` x `height` logical.
pub fn content(width: f32, height: f32, scale: f32) -> Geometry {
    let top = strip_height(scale);
    Geometry {
        x: 0.0,
        y: top,
        w: width.max(0.0),
        h: (height - top).max(0.0),
    }
}

/// One split, with the shared edge on a whole physical pixel. The two halves
/// always sum to `bounds` exactly, so nested rows and columns of fixed sizes
/// tile it with no gap and no overlap.
pub fn split(dir: SplitDir, ratio: f32, bounds: Geometry, scale: f32) -> (Geometry, Geometry) {
    let mut a = bounds;
    let mut b = bounds;
    match dir {
        SplitDir::Vertical => {
            let edge = snap(bounds.x + bounds.w * ratio, scale).clamp(bounds.x, bounds.x + bounds.w);
            a.w = edge - bounds.x;
            b.x = edge;
            b.w = bounds.x + bounds.w - edge;
        }
        SplitDir::Horizontal => {
            let edge = snap(bounds.y + bounds.h * ratio, scale).clamp(bounds.y, bounds.y + bounds.h);
            a.h = edge - bounds.y;
            b.y = edge;
            b.h = bounds.y + bounds.h - edge;
        }
    }
    (a, b)
}

/// Every pane's outer rectangle (border included), in tree order.
pub fn panes(node: &Node, bounds: Geometry, scale: f32) -> Vec<(u64, Geometry)> {
    let mut out = Vec::new();
    place(node, bounds, scale, &mut out);
    out
}

fn place(node: &Node, bounds: Geometry, scale: f32, out: &mut Vec<(u64, Geometry)>) {
    match node {
        Node::Leaf(id) => out.push((*id, bounds)),
        Node::Split {
            dir,
            ratio,
            first,
            second,
        } => {
            let (a, b) = split(*dir, *ratio, bounds, scale);
            place(first, a, scale, out);
            place(second, b, scale, out);
        }
    }
}

/// Columns and rows that fit inside a pane's border, for a PHYSICAL cell of
/// `cell` pixels.
///
/// Computed in physical pixels, not by dividing logical sizes: `cell / scale`
/// is rarely exact, and a width of exactly 80 cells must give 80 columns, not
/// 79 because 80 × (19 / 2.5) came out a hair over the width.
pub fn grid(pane: Geometry, cell: (u32, u32), scale: f32) -> (u16, u16) {
    let (cell_width, cell_height) = (cell.0.max(1), cell.1.max(1));
    let inset = 2.0 * border(scale);
    let fit = |logical: f32, cell: u32| (((logical - inset).max(0.0) * scale + 0.01) / cell as f32) as u32;
    let cols = fit(pane.w, cell_width)
        .clamp(2, u32::from(MAX_COLS).min(MAX_TEXTURE / cell_width).max(2));
    let rows = fit(pane.h, cell_height)
        .clamp(1, u32::from(MAX_ROWS).min(MAX_TEXTURE / cell_height).max(1));
    (cols as u16, rows as u16)
}

/// A real PTY-backed tab set with default settings, for tests. The core's
/// own `TabSet::new` is `cfg(test)` inside the core and so not reachable here.
#[cfg(test)]
pub fn test_tabs() -> TabSet {
    TabSet::with_session(
        cosmix_term_core::config::Settings {
            config: cosmix_term_core::config::Config::default(),
            term: "xterm-256color",
        },
        None,
    )
    .expect("a Mix PTY (/opt/cosmix/bin/mix)")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f32, y: f32, w: f32, h: f32) -> Geometry {
        Geometry { x, y, w, h }
    }

    fn leaf(id: u64) -> Box<Node> {
        Box::new(Node::Leaf(id))
    }

    fn on_pixel_grid(value: f32, scale: f32) -> bool {
        ((value * scale) - (value * scale).round()).abs() < 1e-3
    }

    /// A 2.5x output is where half-pixel edges come from: 900 logical wide
    /// halves to 450 logical = 1125 physical, fine; but a third of it does
    /// not land on a whole pixel, and neither does a ratio a Bus caller set.
    #[test]
    fn split_edges_land_on_whole_physical_pixels_and_tile_exactly() {
        let bounds = rect(0.0, 30.0, 900.0, 530.0);
        for scale in [1.0, 1.25, 1.5, 2.0, 2.5] {
            for ratio in [0.5, 1.0 / 3.0, 0.37, 0.9] {
                for dir in [SplitDir::Vertical, SplitDir::Horizontal] {
                    let (a, b) = split(dir, ratio, bounds, scale);
                    let (edge, far) = match dir {
                        SplitDir::Vertical => (b.x, a.w + b.w),
                        SplitDir::Horizontal => (b.y, a.h + b.h),
                    };
                    assert!(on_pixel_grid(edge, scale), "{dir:?} {ratio} @{scale}: edge {edge}");
                    let whole = match dir {
                        SplitDir::Vertical => bounds.w,
                        SplitDir::Horizontal => bounds.h,
                    };
                    assert!((far - whole).abs() < 1e-3, "halves must sum to the whole");
                }
            }
        }
    }

    /// The tree the Bus builds (split, then split the right half) must come
    /// out as three panes that cover the content area with nothing left over.
    #[test]
    fn nested_splits_cover_the_content_area() {
        let tree = Node::Split {
            dir: SplitDir::Vertical,
            ratio: 0.5,
            first: leaf(1),
            second: Box::new(Node::Split {
                dir: SplitDir::Horizontal,
                ratio: 0.5,
                first: leaf(2),
                second: leaf(3),
            }),
        };
        let bounds = content(900.0, 560.0, 1.0);
        assert_eq!(bounds, rect(0.0, STRIP_HEIGHT, 900.0, 560.0 - STRIP_HEIGHT));
        let placed = panes(&tree, bounds, 1.0);
        assert_eq!(
            placed,
            vec![
                (1, rect(0.0, 30.0, 450.0, 530.0)),
                (2, rect(450.0, 30.0, 450.0, 265.0)),
                (3, rect(450.0, 295.0, 450.0, 265.0)),
            ]
        );
        let area: f32 = placed.iter().map(|(_, g)| g.w * g.h).sum();
        assert_eq!(area, bounds.w * bounds.h);
    }

    #[test]
    fn the_grid_fits_inside_the_border_in_physical_pixels() {
        // 8x16 cells at scale 1 with a 1 px border: 802 wide holds exactly
        // 100 columns, 801 holds 99.
        assert_eq!(grid(rect(0.0, 0.0, 802.0, 482.0), (8, 16), 1.0), (100, 30));
        assert_eq!(grid(rect(0.0, 0.0, 801.0, 481.0), (8, 16), 1.0), (99, 29));
        // 2.5x: border is 3 physical (1.2 logical); 20x40 physical cells.
        // (400 - 2.4) * 2.5 = 994 -> 49 columns.
        assert_eq!(grid(rect(0.0, 0.0, 400.0, 200.0), (20, 40), 2.5), (49, 12));
        // A pane too small for anything still gets a legal PTY size.
        assert_eq!(grid(rect(0.0, 0.0, 3.0, 3.0), (8, 16), 1.0), (2, 1));
        // And a huge one is held under the texture limit.
        assert_eq!(grid(rect(0.0, 0.0, 9000.0, 9000.0), (8, 16), 1.0), (240, 100));
        assert_eq!(grid(rect(0.0, 0.0, 9000.0, 9000.0), (40, 80), 1.0), (102, 51));
    }

    #[test]
    fn a_border_is_whole_physical_pixels_and_never_zero() {
        for scale in [0.5, 1.0, 1.25, 1.5, 2.0, 2.5, 3.0] {
            let border = border(scale);
            assert!(border * scale >= 1.0 - 1e-6);
            assert!(on_pixel_grid(border, scale), "border {border} @{scale}");
        }
    }

    /// The shape `view` draws comes from the real tab set, so a Bus
    /// `term.tab.new` or `term.pane.split` shows up with no keyboard involved.
    #[test]
    fn the_shape_follows_the_tab_set() {
        let mut tabs = test_tabs();
        let first = Shape::of(&tabs);
        assert_eq!(first.tabs.len(), 1);
        assert!(first.tabs[0].active);
        assert_eq!(first.visible(), vec![first.active_pane]);

        let split = tabs.split_active(SplitDir::Vertical).expect("split");
        let shape = Shape::of(&tabs);
        assert_eq!(shape.visible(), vec![first.active_pane, split]);
        assert_eq!(shape.active_pane, split, "a split focuses the new pane");
        assert!(matches!(shape.tree, Some(Node::Split { dir: SplitDir::Vertical, .. })));

        let second = tabs.open().expect("new tab");
        let shape = Shape::of(&tabs);
        assert_eq!(shape.tabs.len(), 2);
        assert!(shape.tabs.iter().any(|tab| tab.id == second && tab.active));
        assert_eq!(shape.visible().len(), 1, "only the active tab's panes are on screen");
        assert!(!shape.visible().contains(&split));

        let removed = tabs.shutdown();
        assert_eq!(Shape::of(&tabs), Shape::default());
        drop(removed);
    }
}
