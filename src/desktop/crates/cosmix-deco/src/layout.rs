//! Chrome layout and hit-testing.
//!
//! `ChromeLayout::compute` turns a theme + client content size into concrete
//! rectangles, all in logical pixels. Three coordinate spaces are in play and
//! must not be conflated:
//!
//! - **Client space (xdg window geometry).** What the client and
//!   `xdg_surface.set_window_geometry` describe, and the space xdg configure
//!   sizes refer to. Server-side chrome is deliberately *outside* it — SSD
//!   never changes what the client calls its window.
//! - **Outer-frame space.** The decorated window: titlebar + borders +
//!   content, excluding shadow. Every rect in `ChromeLayout` lives here,
//!   origin at the frame's top-left. The client content sits at
//!   `content_offset()` = `(extents.left, extents.top)`; the compositor
//!   applies that translation to the client surface exactly once, and
//!   converts sizes between the two spaces with [`DecoExtents`].
//! - **Shadow bounds.** `shadow` extends outside the frame into negative
//!   coordinates; it is never part of either geometry.
//!
//! cosmix-comp positions one `DecoRoot` entity at the frame origin and
//! parents these rects under it as quads, so window moves are one transform
//! write — the same parenting model `SurfaceEntities` already uses for
//! subsurfaces.

use crate::geom::{rect, vec2, Rect, Vec2};
use crate::theme::{ButtonShape, ButtonSide, CaptionButton, DecoTheme, TitleAlign};

/// Extra size the decorations add around the client content. The compositor
/// uses this when converting between client sizes (xdg window geometry — what
/// configure events carry) and outer-frame sizes in both directions.
///
/// Sizes are logical pixels and, for xdg round-trips, expected to be
/// non-negative integers small enough that the size *plus extents* is
/// still exact in `f32` (sums < 2^24 — any real screen dimension) —
/// conversion is exact on that domain (`extents_roundtrip` pins it). `content_size_for_window` saturates to
/// 1×1 when the outer size is smaller than the chrome itself; callers that
/// need to *reject* undersized constraints instead should compare
/// content-space constraints against `ChromeLayout::min_content_size` and
/// outer-space constraints against
/// `window_size_for_content(min_content_size)`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct DecoExtents {
    pub left: f32,
    pub right: f32,
    pub top: f32,
    pub bottom: f32,
}

impl DecoExtents {
    pub fn of(theme: &DecoTheme) -> DecoExtents {
        let b = theme.metrics.border_thickness;
        DecoExtents { left: b, right: b, top: theme.metrics.titlebar_height + b, bottom: b }
    }

    pub fn window_size_for_content(&self, content: Vec2) -> Vec2 {
        vec2(content.x + self.left + self.right, content.y + self.top + self.bottom)
    }

    pub fn content_size_for_window(&self, window: Vec2) -> Vec2 {
        vec2(
            (window.x - self.left - self.right).max(1.0),
            (window.y - self.top - self.bottom).max(1.0),
        )
    }
}

/// Window edges/corners for interactive resize. Values chosen to be trivially
/// mappable to `xdg_toplevel::ResizeEdge`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResizeEdge {
    Top,
    Bottom,
    Left,
    Right,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

/// What lives under a point, from the compositor input router's perspective.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ChromePart {
    /// Forward to the client surface.
    Content,
    /// Start an interactive move on press; toggle maximize on double-click.
    TitlebarDrag,
    Button(CaptionButton),
    /// Start an interactive resize on press; also selects the cursor shape.
    Resize(ResizeEdge),
    /// Outside the window and its resize band entirely.
    Outside,
}

/// All chrome rectangles for one window at one size.
#[derive(Clone, Debug, PartialEq)]
pub struct ChromeLayout {
    /// Full window geometry (titlebar + borders + content), origin 0,0.
    pub window: Rect,
    pub titlebar: Rect,
    pub content: Rect,
    /// Buttons in the same order as `theme.buttons.order`.
    pub buttons: [(CaptionButton, Rect); 3],
    /// Bounding rectangle of all caption buttons, including the gaps between them.
    pub button_cluster: Rect,
    /// Where the title text may be drawn (already clear of the buttons).
    pub title_slot: Rect,
    /// Shadow quad, extending outside `window` (negative origin).
    pub shadow: Rect,
    resize_band: f32,
    corner_zone: f32,
}

impl ChromeLayout {
    /// Smallest content size the chrome can wrap: below it the caption
    /// cluster would not fit inside the titlebar. `compute` sizes the frame
    /// by at least this much so every button stays visible and hit-testable.
    /// Integrations should clamp xdg configure sizes with this before
    /// sending them to clients (in content space; for outer-space
    /// constraints compare against `window_size_for_content` of this) —
    /// but must still pass the client's *committed* size to `compute`.
    pub fn min_content_size(theme: &DecoTheme) -> Vec2 {
        let c = &theme.buttons;
        let (bw, gap) = match c.shape {
            ButtonShape::Circle { diameter } => (diameter, c.gap),
            ButtonShape::FullHeightRect { width } => (width, 0.0),
        };
        // The cluster measured from its window edge; the titlebar (whose
        // width equals the content width) must hold all of it.
        vec2((c.edge_inset + 3.0 * bw + 2.0 * gap).max(1.0), 1.0)
    }

    /// `content_size` is the client's *committed* size — what the client
    /// actually made its window, which may be smaller than any minimum we
    /// asked for in a configure (configure sizes are suggestions; committed
    /// buffers are the truth, including transiently during resizes). The
    /// chrome keeps its minimum footprint regardless, but `content` stays
    /// the committed size: the uncovered interior hit-tests as frame
    /// (resize), never as `Content` — no point is ever forwarded to a
    /// client outside its own buffer.
    pub fn compute(theme: &DecoTheme, content_size: Vec2) -> ChromeLayout {
        let m = &theme.metrics;
        let ext = DecoExtents::of(theme);
        let min = Self::min_content_size(theme);
        let chrome_size = vec2(content_size.x.max(min.x), content_size.y.max(min.y));
        let content_size = vec2(content_size.x.max(1.0), content_size.y.max(1.0));
        let win = ext.window_size_for_content(chrome_size);
        let window = rect(0.0, 0.0, win.x, win.y);
        let titlebar = rect(m.border_thickness, m.border_thickness, win.x - 2.0 * m.border_thickness, m.titlebar_height);
        let content = rect(ext.left, ext.top, content_size.x, content_size.y);

        let c = &theme.buttons;
        let mut buttons = [(CaptionButton::Close, Rect::default()); 3];
        let (bw, bh, gap) = match c.shape {
            ButtonShape::Circle { diameter } => (diameter, diameter, c.gap),
            ButtonShape::FullHeightRect { width } => (width, titlebar.h, 0.0),
        };
        let by = titlebar.y + (titlebar.h - bh) / 2.0;
        for (i, &kind) in c.order.iter().enumerate() {
            let offset = c.edge_inset + i as f32 * (bw + gap);
            let bx = match c.side {
                ButtonSide::Left => titlebar.x + offset,
                ButtonSide::Right => titlebar.x + titlebar.w - offset - bw,
            };
            buttons[i] = (kind, rect(bx, by, bw, bh));
        }
        let cluster_left = buttons
            .iter()
            .map(|(_, button)| button.x)
            .fold(f32::INFINITY, f32::min);
        let cluster_top = buttons
            .iter()
            .map(|(_, button)| button.y)
            .fold(f32::INFINITY, f32::min);
        let cluster_right = buttons
            .iter()
            .map(|(_, button)| button.x + button.w)
            .fold(f32::NEG_INFINITY, f32::max);
        let cluster_bottom = buttons
            .iter()
            .map(|(_, button)| button.y + button.h)
            .fold(f32::NEG_INFINITY, f32::max);
        let button_cluster = rect(
            cluster_left,
            cluster_top,
            cluster_right - cluster_left,
            cluster_bottom - cluster_top,
        );

        // The span the button cluster occupies from its window edge.
        let cluster_span = c.edge_inset + 3.0 * bw + 2.0 * gap + m.title_pad;
        let title_slot = match (m.title_align, c.side) {
            // Centered titles keep symmetric margins so the text stays truly
            // centred; the margin is the cluster span on both sides.
            (TitleAlign::Center, _) => rect(
                titlebar.x + cluster_span,
                titlebar.y,
                (titlebar.w - 2.0 * cluster_span).max(0.0),
                titlebar.h,
            ),
            (TitleAlign::Leading, ButtonSide::Left) => rect(
                titlebar.x + cluster_span,
                titlebar.y,
                (titlebar.w - cluster_span - m.title_pad).max(0.0),
                titlebar.h,
            ),
            (TitleAlign::Leading, ButtonSide::Right) => rect(
                titlebar.x + m.title_pad,
                titlebar.y,
                (titlebar.w - cluster_span - m.title_pad).max(0.0),
                titlebar.h,
            ),
        };

        let s = &m.shadow;
        let shadow = window.inflate(s.softness);
        let shadow = rect(shadow.x, shadow.y + s.offset_y, shadow.w, shadow.h);

        ChromeLayout {
            window,
            titlebar,
            content,
            buttons,
            button_cluster,
            title_slot,
            shadow,
            resize_band: m.resize_band,
            // Corner zones get a slightly larger diagonal reach so corners are
            // grabbable even with a thin band.
            corner_zone: (m.resize_band * 2.0).max(12.0),
        }
    }

    /// Classify a point in outer-frame space. Priority: buttons → titlebar →
    /// content → visible frame / uncovered interior → resize band → outside.
    /// Buttons win over the drag area. Resize hits come from two places: the
    /// invisible band just *outside* the window edge (the mac/win11 grab
    /// feel), and any in-window point that is neither titlebar nor committed
    /// client content — win11's 1px hairline, or interior the client's
    /// committed buffer doesn't cover. Both are compositor-owned; forwarding
    /// them would hand the client coordinates outside its surface.
    ///
    /// The test is purely geometric: rounded-corner transparency (the Phase-3
    /// SDF mask) is not modelled, so a click in a masked-away corner still
    /// hits chrome. Accepted for now — the zone is a few pixels per corner.
    pub fn hit_test(&self, p: Vec2) -> ChromePart {
        if self.window.contains(p) {
            for &(kind, r) in &self.buttons {
                if r.contains(p) {
                    return ChromePart::Button(kind);
                }
            }
            if self.titlebar.contains(p) {
                return ChromePart::TitlebarDrag;
            }
            if self.content.contains(p) {
                return ChromePart::Content;
            }
            // Visible frame border or interior uncovered by the committed
            // buffer: pick the edge by which side of the content/titlebar
            // block the point falls on. For borderless styles at or above
            // the minimum size, titlebar + content tile the window and this
            // is unreachable.
            let (near_l, near_r) = (p.x < self.content.x, p.x >= self.content.x + self.content.w);
            let (near_t, near_b) = (p.y < self.titlebar.y, p.y >= self.content.y + self.content.h);
            let edge = match (near_l, near_r, near_t, near_b) {
                (true, _, true, _) => ResizeEdge::TopLeft,
                (_, true, true, _) => ResizeEdge::TopRight,
                (true, _, _, true) => ResizeEdge::BottomLeft,
                (_, true, _, true) => ResizeEdge::BottomRight,
                (true, ..) => ResizeEdge::Left,
                (_, true, ..) => ResizeEdge::Right,
                (_, _, true, _) => ResizeEdge::Top,
                (_, _, _, true) => ResizeEdge::Bottom,
                _ => return ChromePart::Content, // unreachable: in window, outside all parts
            };
            return ChromePart::Resize(edge);
        }

        let band = self.window.inflate(self.resize_band);
        if !band.contains(p) {
            return ChromePart::Outside;
        }
        let cz = self.corner_zone;
        let (l, t) = (self.window.x, self.window.y);
        let (r, b) = (l + self.window.w, t + self.window.h);
        let near_left = p.x < l + cz;
        let near_right = p.x >= r - cz;
        let near_top = p.y < t + cz;
        let near_bottom = p.y >= b - cz;
        let edge = match (p.x < l, p.x >= r, p.y < t, p.y >= b) {
            (true, _, _, _) if near_top => ResizeEdge::TopLeft,
            (true, _, _, _) if near_bottom => ResizeEdge::BottomLeft,
            (true, _, _, _) => ResizeEdge::Left,
            (_, true, _, _) if near_top => ResizeEdge::TopRight,
            (_, true, _, _) if near_bottom => ResizeEdge::BottomRight,
            (_, true, _, _) => ResizeEdge::Right,
            (_, _, true, _) if near_left => ResizeEdge::TopLeft,
            (_, _, true, _) if near_right => ResizeEdge::TopRight,
            (_, _, true, _) => ResizeEdge::Top,
            (_, _, _, true) if near_left => ResizeEdge::BottomLeft,
            (_, _, _, true) if near_right => ResizeEdge::BottomRight,
            (_, _, _, true) => ResizeEdge::Bottom,
            _ => return ChromePart::Outside, // unreachable: band ∧ ¬window ⇒ one side true
        };
        ChromePart::Resize(edge)
    }

    /// Where the client surface sits inside the outer frame — the one
    /// translation the compositor applies between client space and frame
    /// space (equal to `(extents.left, extents.top)`).
    pub fn content_offset(&self) -> Vec2 {
        vec2(self.content.x, self.content.y)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presets;
    use crate::theme::{Mode, Scheme};

    fn themes() -> Vec<DecoTheme> {
        let mut v = vec![];
        for mode in [Mode::Dark, Mode::Light] {
            v.push(presets::mac(mode));
            v.push(presets::win11(mode));
            for scheme in Scheme::ALL {
                v.push(presets::cosmix(scheme, mode));
            }
        }
        v
    }

    #[test]
    fn buttons_sit_inside_titlebar() {
        for theme in themes() {
            let l = ChromeLayout::compute(&theme, vec2(640.0, 480.0));
            for (kind, r) in l.buttons {
                assert!(r.x >= l.titlebar.x - 0.01, "{:?} {kind:?} left", theme.style);
                assert!(r.x + r.w <= l.titlebar.x + l.titlebar.w + 0.01, "{:?} {kind:?} right", theme.style);
                assert!(r.y >= l.titlebar.y - 0.01 && r.y + r.h <= l.titlebar.y + l.titlebar.h + 0.01,
                    "{:?} {kind:?} vertical", theme.style);
            }
        }
    }

    #[test]
    fn buttons_do_not_overlap() {
        for theme in themes() {
            let l = ChromeLayout::compute(&theme, vec2(640.0, 480.0));
            for i in 0..3 {
                for j in (i + 1)..3 {
                    let (a, b) = (l.buttons[i].1, l.buttons[j].1);
                    let overlap = a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h;
                    assert!(!overlap, "{:?}: buttons {i} and {j} overlap", theme.style);
                }
            }
        }
    }

    #[test]
    fn button_cluster_contains_buttons_and_inter_button_gaps() {
        let theme = presets::mac(Mode::Light);
        let layout = ChromeLayout::compute(&theme, vec2(640.0, 480.0));

        for (_, button) in layout.buttons {
            assert!(layout.button_cluster.contains(button.center()));
        }
        for pair in layout.buttons.windows(2) {
            let left = pair[0].1;
            let right = pair[1].1;
            let gap = vec2(
                (left.x + left.w + right.x) / 2.0,
                layout.button_cluster.center().y,
            );
            assert!(layout.button_cluster.contains(gap));
            assert!(
                layout.buttons.iter().all(|(_, button)| !button.contains(gap)),
                "probe must exercise a cluster gap"
            );
        }
    }

    #[test]
    fn extents_roundtrip() {
        for theme in themes() {
            let ext = DecoExtents::of(&theme);
            let content = vec2(800.0, 600.0);
            assert_eq!(ext.content_size_for_window(ext.window_size_for_content(content)), content);
        }
    }

    #[test]
    fn hit_test_priorities() {
        for theme in themes() {
            let l = ChromeLayout::compute(&theme, vec2(640.0, 480.0));
            // Every button centre hits that button.
            for (kind, r) in l.buttons {
                assert_eq!(l.hit_test(r.center()), ChromePart::Button(kind), "{:?}", theme.style);
            }
            // Titlebar midpoint (clear of buttons for a 640px window) drags.
            assert_eq!(l.hit_test(l.titlebar.center()), ChromePart::TitlebarDrag);
            // Content centre forwards to the client.
            assert_eq!(l.hit_test(l.content.center()), ChromePart::Content);
            // Just outside each edge resizes.
            let m = 2.0;
            assert_eq!(l.hit_test(vec2(l.window.w / 2.0, -m)), ChromePart::Resize(ResizeEdge::Top));
            assert_eq!(l.hit_test(vec2(l.window.w / 2.0, l.window.h + m)), ChromePart::Resize(ResizeEdge::Bottom));
            assert_eq!(l.hit_test(vec2(-m, l.window.h / 2.0)), ChromePart::Resize(ResizeEdge::Left));
            assert_eq!(l.hit_test(vec2(l.window.w + m, l.window.h / 2.0)), ChromePart::Resize(ResizeEdge::Right));
            assert_eq!(l.hit_test(vec2(-m, -m)), ChromePart::Resize(ResizeEdge::TopLeft));
            assert_eq!(l.hit_test(vec2(l.window.w + m, l.window.h + m)), ChromePart::Resize(ResizeEdge::BottomRight));
            // Far away is outside.
            assert_eq!(l.hit_test(vec2(-500.0, -500.0)), ChromePart::Outside);
        }
    }

    #[test]
    fn narrow_window_keeps_buttons_inside_and_clickable() {
        for theme in themes() {
            // Far below any plausible minimum: the chrome must keep its
            // footprint, not eject buttons.
            let l = ChromeLayout::compute(&theme, vec2(10.0, 5.0));
            for (kind, r) in l.buttons {
                assert!(
                    r.x >= l.window.x - 0.01 && r.x + r.w <= l.window.x + l.window.w + 0.01,
                    "{:?}: {kind:?} outside the window",
                    theme.style
                );
                assert_eq!(l.hit_test(r.center()), ChromePart::Button(kind), "{:?}", theme.style);
            }
        }
    }

    #[test]
    fn undersized_committed_content_stays_truthful() {
        for theme in themes() {
            let ext = DecoExtents::of(&theme);
            let l = ChromeLayout::compute(&theme, vec2(10.0, 5.0));
            // The content rect reports what the client committed, not the
            // chrome's minimum footprint...
            assert_eq!((l.content.w, l.content.h), (10.0, 5.0), "{:?}", theme.style);
            // ...and interior the buffer doesn't cover is never Content.
            let in_gap = vec2(ext.left + 10.0 + 5.0, ext.top + 2.0);
            assert!(l.window.contains(in_gap), "{:?}: probe point escaped the window", theme.style);
            assert!(
                matches!(l.hit_test(in_gap), ChromePart::Resize(_) | ChromePart::Button(_)),
                "{:?}: uncovered interior forwarded to the client",
                theme.style
            );
        }
    }

    #[test]
    fn frame_border_resizes_instead_of_forwarding_to_client() {
        let t = presets::win11(Mode::Light); // 1px visible border
        let l = ChromeLayout::compute(&t, vec2(800.0, 600.0));
        assert_eq!(l.hit_test(vec2(0.5, 100.0)), ChromePart::Resize(ResizeEdge::Left));
        assert_eq!(l.hit_test(vec2(l.window.w - 0.5, 100.0)), ChromePart::Resize(ResizeEdge::Right));
        assert_eq!(l.hit_test(vec2(400.0, l.window.h - 0.5)), ChromePart::Resize(ResizeEdge::Bottom));
        assert_eq!(
            l.hit_test(vec2(l.window.w - 0.5, l.window.h - 0.5)),
            ChromePart::Resize(ResizeEdge::BottomRight)
        );
        assert_eq!(l.hit_test(vec2(0.5, 0.5)), ChromePart::Resize(ResizeEdge::TopLeft));
    }

    #[test]
    fn content_offset_is_the_extents_corner() {
        for theme in themes() {
            let ext = DecoExtents::of(&theme);
            let l = ChromeLayout::compute(&theme, vec2(640.0, 480.0));
            assert_eq!(l.content_offset(), vec2(ext.left, ext.top), "{:?}", theme.style);
        }
    }

    #[test]
    fn title_slot_clears_buttons() {
        for theme in themes() {
            let l = ChromeLayout::compute(&theme, vec2(640.0, 480.0));
            for (_, b) in l.buttons {
                let overlap = l.title_slot.x < b.x + b.w
                    && b.x < l.title_slot.x + l.title_slot.w
                    && l.title_slot.y < b.y + b.h
                    && b.y < l.title_slot.y + l.title_slot.h;
                assert!(!overlap, "{:?}: title slot overlaps a button", theme.style);
            }
        }
    }
}
