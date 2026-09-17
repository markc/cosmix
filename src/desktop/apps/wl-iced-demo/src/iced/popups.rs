//! Glue: menu panels as `xdg_popup`s, one iced `Surface` each.
//!
//! [`MenuNav`] owns the menu state. After every change the app calls
//! [`MenuPopups::reconcile`], which keeps the popups whose panel chain is
//! unchanged, closes the rest (topmost first, by closing the lowest one that
//! changed) and opens the missing ones: panel 0 anchored below its bar title,
//! deeper panels beside their submenu row.

use super::dropdown::{Dropdown, RowView};
use crate::menus::{Entry, MenuNav, Metrics};
use cosmix_iced_host::core::{Color, Size};
use cosmix_iced_host::{PixelFormat, Settings, Surface};
use cosmix_iced_widgets::MenuStyle;
use cosmix_wl_app::{
    Anchor, ConstraintAdjustment, Ctx, Frame, Gravity, PopupSpec, Rect, SurfaceId, SurfaceInfo,
};

pub type Bar<A> = Vec<(String, Vec<Entry<A>>)>;

struct Panel {
    key: (usize, Vec<usize>),
    id: SurfaceId,
    surface: Surface<Dropdown>,
}

pub struct MenuPopups<A> {
    pub bar: Bar<A>,
    pub nav: MenuNav,
    pub metrics: Metrics,
    pub style: MenuStyle,
    panels: Vec<Panel>,
}

/// Where panel 0 opens: below its bar title, sliding or flipping to stay
/// on the output.
pub fn root_spec(metrics: &Metrics, root: usize, size: (u32, u32)) -> PopupSpec {
    PopupSpec {
        anchor_rect: metrics.bar_item(root),
        size,
        anchor: Anchor::BottomLeft,
        gravity: Gravity::BottomRight,
        constraint: ConstraintAdjustment::SlideX | ConstraintAdjustment::FlipY,
        offset: (0, 0),
        grab: true,
        reactive: false,
    }
}

fn physical(info: &SurfaceInfo) -> Size<u32> {
    Size::new(info.physical.0, info.physical.1)
}

impl<A: Clone> MenuPopups<A> {
    pub fn new(bar: Bar<A>, metrics: Metrics, style: MenuStyle) -> Self {
        Self {
            bar,
            nav: MenuNav::default(),
            metrics,
            style,
            panels: Vec::new(),
        }
    }

    pub fn titles(&self) -> Vec<String> {
        self.bar.iter().map(|(t, _)| t.clone()).collect()
    }

    pub fn level_of(&self, id: SurfaceId) -> Option<usize> {
        self.panels.iter().position(|p| p.id == id)
    }

    pub fn entries(&self, level: usize) -> Option<&[Entry<A>]> {
        self.nav.panel(&self.bar, level)
    }

    /// Bring the popups in line with `nav`. `scale` is the window's.
    pub fn reconcile(&mut self, cx: &mut Ctx<'_>, window: SurfaceId, scale: &SurfaceInfo) {
        let chain = self.nav.chain();
        let keep = self
            .panels
            .iter()
            .zip(&chain)
            .take_while(|(p, key)| p.key == **key)
            .count();
        if keep < self.panels.len() {
            cx.close_popup(self.panels[keep].id);
            self.panels.truncate(keep);
        }
        for (level, key) in chain.into_iter().enumerate().skip(keep) {
            let Some(entries) = self.nav.panel(&self.bar, level) else {
                break;
            };
            let size = self.metrics.panel_size(entries);
            let rows = RowView::from_entries(entries);
            let (parent, spec) = if level == 0 {
                (window, root_spec(&self.metrics, key.0, size))
            } else {
                let parent_entries = self.nav.panel(&self.bar, level - 1).unwrap_or(&[]);
                let row = key.1.last().copied().unwrap_or(0);
                let anchor = if row < parent_entries.len() {
                    self.metrics.row_rect(parent_entries, row)
                } else {
                    Rect::new(0, 0, 1, 1)
                };
                (self.panels[level - 1].id, PopupSpec::submenu(anchor, size))
            };
            let id = match cx.create_popup(parent, &spec) {
                Ok(id) => id,
                Err(e) => {
                    eprintln!("wl-iced-demo: popup: {e}");
                    self.nav.truncate(level);
                    break;
                }
            };
            let info = SurfaceInfo::new(size, scale.scale);
            let surface = Surface::new(
                Dropdown {
                    rows,
                    selected: self.nav.selected(level),
                    metrics: self.metrics,
                    style: self.style,
                },
                Settings {
                    physical_size: physical(&info),
                    scale_factor: info.scale_factor() as f32,
                    background: Some(Color::TRANSPARENT),
                    ..Settings::default()
                },
            );
            self.panels.push(Panel { key, id, surface });
        }
        for (level, panel) in self.panels.iter_mut().enumerate() {
            let selected = self.nav.selected(level);
            if panel.surface.program().selected != selected {
                panel.surface.program_mut().selected = selected;
                cx.request_redraw(panel.id);
            }
        }
    }

    /// A popup was configured or rescaled.
    pub fn configure(&mut self, id: SurfaceId, info: &SurfaceInfo) {
        if let Some(level) = self.level_of(id) {
            let surface = &mut self.panels[level].surface;
            surface.resize(physical(info), info.scale_factor() as f32);
        }
    }

    /// The compositor dismissed popup `id` and those above it.
    pub fn dismissed(&mut self, id: SurfaceId) {
        if let Some(level) = self.level_of(id) {
            self.panels.truncate(level);
            self.nav.truncate(level);
        }
    }

    pub fn close_all(&mut self, cx: &mut Ctx<'_>) {
        self.nav.truncate(0);
        if let Some(first) = self.panels.first() {
            cx.close_popup(first.id);
        }
        self.panels.clear();
    }

    /// Draw popup `frame.surface()`. Returns false if it is not a panel.
    pub fn draw(&mut self, frame: &mut Frame<'_>) -> bool {
        let Some(level) = self.level_of(frame.surface()) else {
            return false;
        };
        let surface = &mut self.panels[level].surface;
        if frame.needs_full_redraw() {
            surface.invalidate();
        }
        let (pixels, width, height, stride) = frame.buffer_mut();
        match surface.draw(pixels, width, height, stride, PixelFormat::Argb8888) {
            Ok(drawn) if drawn.full => frame.commit_full(),
            Ok(drawn) => {
                let rects: Vec<Rect> = drawn
                    .damage
                    .iter()
                    .map(|r| Rect::new(r.x as i32, r.y as i32, r.width as i32, r.height as i32))
                    .collect();
                frame.commit_with_damage(&rects);
            }
            Err(e) => eprintln!("wl-iced-demo: panel draw: {e}"),
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_panel_hangs_below_its_title() {
        let m = Metrics::default();
        let spec = root_spec(&m, 2, (240, 100));
        assert_eq!(spec.anchor_rect, Rect::new(116, 0, 56, 28));
        assert_eq!(spec.anchor, Anchor::BottomLeft);
        assert!(spec.grab);
    }
}
