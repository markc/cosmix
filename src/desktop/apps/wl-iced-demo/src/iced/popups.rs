//! Glue: `cosmix-iced-widgets` menu panels as `xdg_popup`s, one iced
//! `Surface` each.
//!
//! The widget crate owns the menu state (`MenuState`), the navigation
//! (`Navigator`) and the panel widget (`menu::Panel`); this file owns the
//! Wayland side. After every state change the app calls [`MenuPopups::sync`],
//! which fills in the anchors the host is responsible for, then keeps the
//! popups whose panel chain is unchanged, closes the rest (by closing the
//! lowest changed one, which takes those above it) and opens the missing
//! ones: panel 0 anchored to its bar title in the window, deeper panels to
//! their parent row inside the parent panel.

use super::chrome::ChromeMsg;
use cosmix_iced_host::core::{Color, Point, Size};
use cosmix_iced_host::{Element, PixelFormat, Program, Renderer, Settings, Surface, input};
use cosmix_iced_widgets::MenuStyle;
use cosmix_iced_widgets::menu::{self, Item, MenuState, Navigator};
use cosmix_wl_app::{
    Anchor, ButtonState, ConstraintAdjustment, Ctx, Frame, Gravity, PointerEvent, PointerKind,
    PopupSpec, Rect, SurfaceId, SurfaceInfo,
};

/// What a panel surface reports back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelMsg {
    Hover(Option<usize>),
    Press(usize),
}

/// One menu panel, filling its own popup surface.
pub struct PanelProgram {
    items: Vec<Item<ChromeMsg>>,
    selected: Option<usize>,
    style: MenuStyle,
    pending: Vec<PanelMsg>,
}

impl Program for PanelProgram {
    // The panel widget publishes the item message type, so a panel speaks
    // the chrome's language and reports its rows through two of its variants.
    type Message = ChromeMsg;

    fn update(&mut self, message: ChromeMsg) {
        match message {
            ChromeMsg::PanelHover(row) => self.pending.push(PanelMsg::Hover(row)),
            ChromeMsg::PanelPress(row) => self.pending.push(PanelMsg::Press(row)),
            // An item's own message: the host reads it from the navigator,
            // which is the only thing that knows which panel it came from.
            _ => {}
        }
    }

    fn view(&self) -> Element<'_, ChromeMsg> {
        menu::Panel::new(&self.items, self.selected)
            .style(self.style)
            .on_hover(ChromeMsg::PanelHover)
            .on_press(ChromeMsg::PanelPress)
            .into()
    }
}

struct OpenPanel {
    /// The rows leading to this panel; equal keys mean the same popup.
    key: (usize, Vec<Option<usize>>),
    id: SurfaceId,
    surface: Surface<PanelProgram>,
}

pub struct MenuPopups {
    pub style: MenuStyle,
    /// Only for measuring panels; it draws nothing.
    measure: Renderer,
    panels: Vec<OpenPanel>,
}

/// Where panel 0 opens: below its bar title, sliding or flipping to stay on
/// the output.
fn root_spec(anchor: Rect, size: (u32, u32)) -> PopupSpec {
    PopupSpec {
        anchor_rect: anchor,
        size,
        anchor: Anchor::BottomLeft,
        gravity: Gravity::BottomRight,
        constraint: ConstraintAdjustment::SlideX | ConstraintAdjustment::FlipY,
        offset: (0, 0),
        grab: true,
        reactive: false,
    }
}

fn logical_rect(r: cosmix_iced_host::core::Rectangle) -> Rect {
    Rect::new(
        r.x.floor() as i32,
        r.y.floor() as i32,
        r.width.ceil().max(1.0) as i32,
        r.height.ceil().max(1.0) as i32,
    )
}

impl MenuPopups {
    pub fn new(style: MenuStyle) -> Self {
        Self {
            style,
            measure: Renderer::new(
                cosmix_iced_host::core::Font::DEFAULT,
                cosmix_iced_host::core::Pixels(style.text_size),
            ),
            panels: Vec::new(),
        }
    }

    pub fn level_of(&self, id: SurfaceId) -> Option<usize> {
        self.panels.iter().position(|p| p.id == id)
    }

    pub fn is_open(&self) -> bool {
        !self.panels.is_empty()
    }

    fn panel_size(&self, items: &[Item<ChromeMsg>]) -> (u32, u32) {
        let size = menu::panel_size(&self.measure, items, self.style);
        (
            size.width.ceil().max(1.0) as u32,
            size.height.ceil().max(1.0) as u32,
        )
    }

    /// Fills in the anchors only the host can know: the widget publishes the
    /// bar title's rectangle, and every deeper panel hangs off its parent
    /// row, whose position depends on the parent panel's width.
    fn fill_anchors(&self, nav: &Navigator<'_, ChromeMsg>, state: &mut MenuState) {
        while state.anchors.len() < state.path.len() {
            let level = state.anchors.len();
            let Some(parent) = level.checked_sub(1) else {
                // The bar owns anchors[0]; without it nothing can be shown.
                return;
            };
            let items = nav.panel(state, parent);
            let width = self.panel_size(items).0 as f32;
            let Some(row) = state.path[parent] else {
                return;
            };
            let Some(bounds) = menu::row_bounds(items, row, width, self.style) else {
                return;
            };
            state.anchors.push(bounds);
        }
    }

    /// Bring the popups in line with the menu state.
    pub fn sync(
        &mut self,
        cx: &mut Ctx<'_>,
        window: SurfaceId,
        info: &SurfaceInfo,
        nav: &Navigator<'_, ChromeMsg>,
        state: &mut MenuState,
    ) {
        self.fill_anchors(nav, state);
        let specs = nav.open_panels(state);
        let key = |level: usize| (state.root.unwrap_or(0), state.path[..level].to_vec());
        let keep = self
            .panels
            .iter()
            .zip(&specs)
            .take_while(|(panel, spec)| panel.key == key(spec.level))
            .count();
        if keep < self.panels.len() {
            cx.close_popup(self.panels[keep].id);
            self.panels.truncate(keep);
        }
        for spec in specs.iter().skip(keep) {
            let size = self.panel_size(spec.items);
            let anchor = logical_rect(spec.anchor);
            let (parent, popup) = match spec.level.checked_sub(1) {
                None => (window, root_spec(anchor, size)),
                Some(above) => (self.panels[above].id, PopupSpec::submenu(anchor, size)),
            };
            let id = match cx.create_popup(parent, &popup) {
                Ok(id) => id,
                Err(e) => {
                    eprintln!("wl-iced-demo: popup: {e}");
                    break;
                }
            };
            let program = PanelProgram {
                items: spec.items.to_vec(),
                selected: spec.selected,
                style: self.style,
                pending: Vec::new(),
            };
            let physical = SurfaceInfo::new(size, info.scale);
            let surface = Surface::new(
                program,
                Settings {
                    physical_size: Size::new(physical.physical.0, physical.physical.1),
                    scale_factor: physical.scale_factor() as f32,
                    background: Some(Color::TRANSPARENT),
                    ..Settings::default()
                },
            );
            self.panels.push(OpenPanel {
                key: key(spec.level),
                id,
                surface,
            });
        }
        for (panel, spec) in self.panels.iter_mut().zip(&specs) {
            if panel.surface.program().selected != spec.selected {
                panel.surface.program_mut().selected = spec.selected;
                cx.request_redraw(panel.id);
            }
        }
    }

    /// A popup was configured or rescaled.
    pub fn configure(&mut self, id: SurfaceId, info: &SurfaceInfo) {
        if let Some(level) = self.level_of(id) {
            self.panels[level].surface.resize(
                Size::new(info.physical.0, info.physical.1),
                info.scale_factor() as f32,
            );
        }
    }

    /// The compositor dismissed popup `id` and those above it.
    pub fn dismissed(&mut self, id: SurfaceId) -> Option<usize> {
        let level = self.level_of(id)?;
        self.panels.truncate(level);
        Some(level)
    }

    pub fn close_all(&mut self, cx: &mut Ctx<'_>) {
        if let Some(first) = self.panels.first() {
            cx.close_popup(first.id);
        }
        self.panels.clear();
    }

    /// Feed a pointer event to panel `level` and return what it reported.
    pub fn pointer(
        &mut self,
        cx: &mut Ctx<'_>,
        level: usize,
        event: &PointerEvent,
    ) -> Vec<PanelMsg> {
        let Some(panel) = self.panels.get_mut(level) else {
            return Vec::new();
        };
        let point = Point::new(event.position.0 as f32, event.position.1 as f32);
        match event.kind {
            PointerKind::Enter | PointerKind::Motion => panel.surface.cursor_moved(point),
            PointerKind::Leave => panel.surface.cursor_left(),
            PointerKind::Button { button, state } => {
                panel.surface.set_cursor(Some(point));
                panel
                    .surface
                    .queue_event(input::button_event(button, state == ButtonState::Pressed));
            }
            PointerKind::Axis { .. } => return Vec::new(),
        }
        let update = panel.surface.process();
        if update.needs_redraw {
            cx.request_redraw(panel.id);
        }
        std::mem::take(&mut panel.surface.program_mut().pending)
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
    fn a_root_panel_hangs_below_its_title() {
        let spec = root_spec(Rect::new(116, 0, 56, 28), (240, 100));
        assert_eq!(spec.anchor_rect, Rect::new(116, 0, 56, 28));
        assert_eq!(spec.anchor, Anchor::BottomLeft);
        assert!(spec.grab);
    }

    #[test]
    fn logical_rects_round_outward() {
        let r =
            cosmix_iced_host::core::Rectangle::new(Point::new(4.6, 28.2), Size::new(55.1, 27.4));
        assert_eq!(logical_rect(r), Rect::new(4, 28, 56, 28));
    }
}
