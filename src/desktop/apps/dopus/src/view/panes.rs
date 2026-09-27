//! The twin panes: one column per [`PaneId`] — pane header (nav buttons +
//! location bar) · sort header · [`rows::FileList`] — split by a draggable
//! [`Divider`]. Pane widths come from the core's live `split_ratio` (Fill
//! portions, so the core stays the single source of truth: persistence
//! derives from core state only, the app-contract law 7); the active pane
//! carries the accent border and the stronger header text.
//!
//! The pane controls publish [`Msg::Pane`] (activate-then-act: the core's
//! `go_back`/`set_sort`/… act on the active pane, so clicking an inactive
//! pane's button first activates that pane — one code path, no pane-targeted
//! duplicates of core verbs). The listing is pane-agnostic: its messages are
//! mapped onto [`Msg::PaneRows`] with the pane id riding the message.

use std::time::{Duration, Instant};

use iced::advanced::widget::{Tree, tree};
use iced::advanced::{Clipboard, Layout, Renderer as _, Shell, Widget, layout, mouse, renderer};
use iced::widget::{button, column, container, row, text};
use iced::{Element, Event, Length, Rectangle, Size};

use cosmix_dopus_core::{PaneId, PaneModel, VisibleRow};
use iced_tiny_skia::Renderer;

use crate::app::{Msg, PaneOp};
use crate::icons::{Icon, Icons};
use crate::view::{Look, location, rows};

/// A second press on the divider inside this window is a double-click
/// (reset to exactly 0.5).
const DOUBLE_CLICK: Duration = Duration::from_millis(400);
/// The split clamp, matching the divider drag contract.
pub const SPLIT_MIN: f32 = 0.1;
pub const SPLIT_MAX: f32 = 0.9;

/// One pane's full column. `portion` is the pane's Fill portion out of 100
/// (the core's live `split_ratio`, quantised); `active` drives the border
/// and header weight; `editing` is `Some(text)` only for the pane whose
/// location bar is being edited (the real path text, never sanitised).
// The widget bundle needs each of these; bundling into another params struct
// would just rename the nine (ced's editor/draw.rs precedent for the allow).
#[allow(clippy::too_many_arguments)]
pub fn pane_column<'a>(
    look: Look,
    icons: &'a Icons,
    tint: &'a str,
    pane_id: PaneId,
    pane: &'a PaneModel,
    pane_rows: &'a [VisibleRow],
    portion: u16,
    active: bool,
    editing: Option<&'a str>,
) -> Element<'a, Msg> {
    container(
        column![
            pane_header(look, icons, tint, pane, pane_id, active, editing),
            sort_header(look, pane, pane_id),
            Element::new(rows::FileList::new(
                pane_rows,
                pane.selected.as_deref(),
                &pane.path,
                &pane.expanded,
                icons,
                tint,
                look,
            ))
            .map(move |m| Msg::PaneRows(pane_id, m)),
        ]
        .width(Length::Fill)
        .height(Length::Fill),
    )
    .width(Length::FillPortion(portion))
    .height(Length::Fill)
    .into()
}

/// The pane's header strip: back / forward / parent / home / refresh /
/// toggle-hidden icon buttons, then the location bar. An inactive pane's
/// buttons still work (they activate the pane first) but its caption is
/// muted.
fn pane_header<'a>(
    look: Look,
    icons: &'a Icons,
    tint: &'a str,
    pane: &'a PaneModel,
    pane_id: PaneId,
    active: bool,
    editing: Option<&'a str>,
) -> Element<'a, Msg> {
    let icon_button = |icon: Icon, op: PaneOp| {
        let style = crate::view::button_look(&look);
        button(crate::view::image_widget(look, icons, tint, icon))
            .padding(look.chrome.small)
            .on_press_maybe(availability(pane, &op).then_some(Msg::Pane(pane_id, op)))
            .style(style)
    };
    let caption_color = if active {
        look.tokens.primary_text
    } else {
        look.tokens.muted_text
    };
    container(
        column![
            row![
                icon_button(Icon::ArrowLeft, PaneOp::NavBack),
                icon_button(Icon::ArrowRight, PaneOp::NavForward),
                icon_button(Icon::ArrowUp, PaneOp::NavParent),
                icon_button(Icon::House, PaneOp::NavHome),
                icon_button(Icon::Refresh, PaneOp::Refresh),
                icon_button(
                    if pane.show_hidden {
                        Icon::EyeOff
                    } else {
                        Icon::Eye
                    },
                    PaneOp::ToggleHidden
                ),
                text(cosmix_dopus_core::sanitise_display_path(&pane.path))
                    .font(look.mono_font)
                    .size(look.mono_px * 0.9)
                    .color(caption_color),
            ]
            .spacing(look.chrome.small)
            .align_y(iced::Alignment::Center),
            location::bar(look, pane, pane_id, editing),
        ]
        .spacing(look.chrome.edge * 2.0),
    )
    .width(Length::Fill)
    .padding([look.chrome.small, look.chrome.pad])
    .align_y(iced::Alignment::Center)
    .style(look.strip(
        if active {
            look.tokens.primary
        } else {
            look.chrome.secondary
        },
        caption_color,
    ))
    .into()
}

/// A pane-local control can only be offered when the core could act: a
/// directory can only be left when there is somewhere to go; the rest are
/// always offered (the core re-checks and status-lines the no-ops).
fn availability(pane: &PaneModel, op: &PaneOp) -> bool {
    if matches!(op, PaneOp::NavParent) {
        return pane.path.parent().is_some();
    }
    true
}

/// The pane's sort headers: the three columns as buttons; the pane's active
/// column shows its direction. A click activates the pane then sorts it
/// (`Msg::Pane`; law 5 — the core adopts a new column ascending and toggles
/// a same-column repeat itself).
fn sort_header<'a>(look: Look, pane: &'a PaneModel, pane_id: PaneId) -> Element<'a, Msg> {
    super::columns::Header {
        look,
        pane: pane_id,
        sort: pane.sort,
        ascending: pane.ascending,
    }
    .into()
}

// -- the divider --------------------------------------------------------------

/// Tree state for the divider: the drag and the double-click tracker.
#[derive(Default)]
struct DividerState {
    /// A press is down (the pointer may leave the 6 px handle while dragging).
    dragging: bool,
    /// The current drag has moved the split: a release after movement is a
    /// drag, not a click — the double-click window runs click-to-click only.
    moved: bool,
    /// The last completed click (a release that did NOT drag): a second
    /// within [`DOUBLE_CLICK`] resets the split to exactly 0.5.
    last_click: Option<Instant>,
}

/// The 6 px drag handle between the panes: press and drag to move the split
/// (publishing [`Msg::Split`] with the ratio clamped to
/// [`SPLIT_MIN`]–[`SPLIT_MAX`]), double-click to restore 0.5. The cursor is
/// col-resize over the handle.
///
/// Geometry uses the root viewport and the same sidebar portions as root's
/// layout, subtracting all open sidebar handles and the pane handle.
pub struct Divider {
    width: f32,
    edge: f32,
    target: Option<cosmix_dopus_core::config::Sidebar>,
    sides: [u16; 2],
    /// The grip colours (tokens; a hover/drag lights the handle with the
    /// active pane's accent).
    border: iced::Color,
    accent: iced::Color,
}

impl Divider {
    pub fn new(
        look: &Look,
        target: Option<cosmix_dopus_core::config::Sidebar>,
        sides: [u16; 2],
    ) -> Self {
        Self {
            width: look.chrome.small + 2.0 * look.chrome.edge,
            edge: look.chrome.edge,
            target,
            sides,
            border: look.tokens.border,
            accent: look.chrome.accent,
        }
    }

    /// The ratio under an absolute cursor x, clamped to the drag contract.
    /// The denominator is the panes row MINUS the divider — the exact space
    /// the two FillPortion panes share in `view::root` — so the grip's
    /// centre tracks the cursor at the clamp extremes too.
    fn message_at(&self, x: f32, viewport: &Rectangle) -> Msg {
        use cosmix_dopus_core::config::Sidebar;
        let handles = self.sides.iter().filter(|p| **p > 0).count() as f32;
        let available = (viewport.width - handles * self.width).max(1.0);
        match self.target {
            Some(Sidebar::Places) => Msg::SidebarWidth(
                Sidebar::Places,
                ((x - viewport.x - self.width / 2.0) / available).clamp(0.1, 0.3),
            ),
            Some(Sidebar::Properties) => Msg::SidebarWidth(
                Sidebar::Properties,
                ((viewport.x + viewport.width - x - self.width / 2.0) / available).clamp(0.1, 0.3),
            ),
            None => {
                let left = viewport.x
                    + available * self.sides[0] as f32 / 1000.0
                    + if self.sides[0] > 0 { self.width } else { 0.0 };
                let width = (available * (1000 - self.sides[0] - self.sides[1]) as f32 / 1000.0
                    - self.width)
                    .max(1.0);
                Msg::Split(((x - left - self.width / 2.0) / width).clamp(SPLIT_MIN, SPLIT_MAX))
            }
        }
    }
}

impl Widget<Msg, iced::Theme, Renderer> for Divider {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<DividerState>()
    }
    fn size(&self) -> Size<Length> {
        Size::new(Length::Fixed(self.width), Length::Fill)
    }

    fn state(&self) -> tree::State {
        tree::State::new(DividerState::default())
    }

    fn layout(
        &mut self,
        _tree: &mut Tree,
        _renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        layout::Node::new(limits.resolve(Length::Fixed(self.width), Length::Fill, Size::ZERO))
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _renderer: &Renderer,
        _clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Msg>,
        viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();
        let Some(clip) = bounds.intersection(viewport) else {
            return;
        };
        let st = tree.state.downcast_mut::<DividerState>();
        match event {
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))
                if cursor.is_over(clip) =>
            {
                if st
                    .last_click
                    .is_some_and(|when| when.elapsed() < DOUBLE_CLICK)
                {
                    // Double-click: exactly half; this press does not start
                    // a drag.
                    st.last_click = None;
                    shell.publish(match self.target {
                        None => Msg::Split(0.5),
                        Some(sidebar) => Msg::SidebarWidth(
                            sidebar,
                            cosmix_dopus_core::config::SidebarConfig::default().width,
                        ),
                    });
                } else {
                    st.dragging = true;
                    st.moved = false;
                }
                shell.capture_event();
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                // The double-click window runs click-to-click: only a
                // release that did NOT drag stamps it, so a drag-and-repress
                // gesture cannot snap the split to 0.5.
                if st.dragging && !st.moved {
                    st.last_click = Some(Instant::now());
                }
                st.dragging = false;
            }
            Event::Mouse(mouse::Event::CursorMoved { position }) if st.dragging => {
                st.moved = true;
                shell.publish(self.message_at(position.x, viewport));
                shell.capture_event();
            }
            // A release outside the window never arrives, and iced's
            // CursorMoved carries no button state, so a held drag cannot be
            // detected as orphaned per-event: losing focus or a resize ends
            // the drag instead (the split keeps its last published ratio).
            Event::Window(iced::window::Event::Unfocused | iced::window::Event::Resized(_)) => {
                st.dragging = false;
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
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();
        let Some(clip) = bounds.intersection(viewport) else {
            return;
        };
        let st = tree.state.downcast_ref::<DividerState>();
        // The grip: a 2 px hairline centred in the 6 px strip. Hovering or
        // dragging lights it with the accent (tokens, zero literals).
        let active = st.dragging || cursor.is_over(clip);
        let color = if active { self.accent } else { self.border };
        let grip = Rectangle {
            x: bounds.center_x() - self.edge / 2.0,
            y: bounds.y,
            width: self.edge,
            height: bounds.height,
        };
        if let Some(clipped) = grip.intersection(&clip) {
            renderer.fill_quad(
                renderer::Quad {
                    bounds: clipped,
                    ..renderer::Quad::default()
                },
                color,
            );
        }
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _viewport: &Rectangle,
        _renderer: &Renderer,
    ) -> mouse::Interaction {
        // The col-resize cursor is the handle's, not the window's: only over
        // the handle itself, or while a drag carries it elsewhere (the
        // cosmix-iced-widgets fader.rs shape).
        let st = tree.state.downcast_ref::<DividerState>();
        if st.dragging || cursor.is_over(layout.bounds()) {
            mouse::Interaction::ResizingHorizontally
        } else {
            mouse::Interaction::None
        }
    }
}

impl<'a> From<Divider> for Element<'a, Msg, iced::Theme, Renderer> {
    fn from(divider: Divider) -> Self {
        Element::new(divider)
    }
}
