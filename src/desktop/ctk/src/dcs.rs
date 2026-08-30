//! Reusable Dual Carousel Sidebars (DCS) application shell.
//!
//! Each side is autonomous: it can be closed, float over the centre, or pin
//! beside it and reduce the centre's available width. Panels on either side
//! form an independently navigable carousel. A narrow window temporarily
//! renders a requested pin as floating without discarding that preference.

use accesskit::Role;
use bevy::a11y::AccessibilityNode;
#[cfg(debug_assertions)]
use bevy::app::PostUpdate;
use bevy::app::{App, Plugin, Update};
use bevy::ecs::entity::Entity;
use bevy::ecs::hierarchy::ChildOf;
#[cfg(debug_assertions)]
use bevy::ecs::hierarchy::Children;
use bevy::ecs::observer::On;
#[cfg(debug_assertions)]
use bevy::ecs::schedule::IntoScheduleConfigs;
use bevy::ecs::system::{Commands, Query};
use bevy::feathers::cursor::EntityCursor;
use bevy::feathers::theme::{ThemeBackgroundColor, ThemeBorderColor, ThemeTextColor};
use bevy::input_focus::tab_navigation::TabIndex;
#[cfg(debug_assertions)]
use bevy::log::warn;
use bevy::picking::events::{Click, Drag, Pointer};
use bevy::picking::hover::Hovered;
use bevy::picking::pointer::PointerButton;
use bevy::picking::Pickable;
use bevy::prelude::{
    default, AlignItems, BackgroundColor, BorderColor, BorderRadius, Button, Color, Component,
    ComputedNode, Display, FlexDirection, GlobalZIndex, JustifyContent, Node, PositionType, Text,
    TextFont, Window,
};
#[cfg(debug_assertions)]
use bevy::ui::UiSystems;
use bevy::ui::{percent, px, UiRect};
use bevy::ui_widgets::Activate;
use bevy::window::{PrimaryWindow, SystemCursorIcon};

const FLOAT_Z: i32 = 50;
const MIN_SIDEBAR_FRACTION: f32 = 0.10;
const MIN_SPLIT_FRACTION: f32 = 0.10;

// Static widths cover CTK's standard icons. Every visual is clamped to the
// toolbar reservation; debug layout also reports when that clamp is active.
pub(crate) const DCS_TOP_CONTROL_EDGE_OFFSET_PX: f32 = 7.0;
pub(crate) const DCS_TOP_CONTROL_GAP_PX: f32 = 5.0;
pub(crate) const DCS_TOP_CONTROL_MIN_WIDTH_PX: f32 = 30.0;
pub(crate) const DCS_TOP_CONTROL_PADDING_X_PX: f32 = 7.0;
pub(crate) const DCS_TOP_CONTROL_TOGGLE_ICON_PX: f32 = 18.0;
pub(crate) const DCS_TOP_CONTROL_PIN_ICON_PX: f32 = 17.0;
pub(crate) const DCS_TOP_CONTROL_CLEARANCE_PX: f32 = 5.0;
pub(crate) const DCS_TOP_CONTROL_TOGGLE_WIDTH_PX: f32 =
    DCS_TOP_CONTROL_TOGGLE_ICON_PX + 2.0 * DCS_TOP_CONTROL_PADDING_X_PX;
pub(crate) const DCS_TOP_CONTROL_PIN_WIDTH_PX: f32 =
    DCS_TOP_CONTROL_PIN_ICON_PX + 2.0 * DCS_TOP_CONTROL_PADDING_X_PX;
pub(crate) const DCS_TOP_CONTROLS_EXTENT_PX: f32 = DCS_TOP_CONTROL_EDGE_OFFSET_PX
    + DCS_TOP_CONTROL_TOGGLE_WIDTH_PX
    + DCS_TOP_CONTROL_GAP_PX
    + DCS_TOP_CONTROL_PIN_WIDTH_PX;
pub(crate) const DCS_TOOLBAR_SAFE_PADDING_PX: f32 = 80.0;
// The slot clips to bound a grossly oversized control, not to shave rounding.
// Its width is auto, so at a fractional scale factor it can round down a
// physical pixel while its children round up — at scale 1.5 that clipped a real
// pixel off the pin control. The clip margin absorbs that; it is logical, so it
// scales with the error it covers.
pub(crate) const DCS_TOP_CONTROL_CLIP_MARGIN_PX: f32 = 2.0;
// The clip edge and the toolbar boundary are rounded to physical pixels
// independently, so a logical-only budget can still overshoot by a fraction of
// a pixel at a fractional scale. One logical pixel of allowance covers it.
pub(crate) const DCS_TOP_CONTROL_ROUNDING_ALLOWANCE_PX: f32 = 1.0;
pub(crate) const DCS_TOP_CONTROL_SLOT_MAX_WIDTH_PX: f32 = DCS_TOOLBAR_SAFE_PADDING_PX
    - DCS_TOP_CONTROL_EDGE_OFFSET_PX
    - DCS_TOP_CONTROL_CLIP_MARGIN_PX
    - DCS_TOP_CONTROL_ROUNDING_ALLOWANCE_PX;

const _: () = {
    assert!(DCS_TOP_CONTROL_TOGGLE_WIDTH_PX >= DCS_TOP_CONTROL_MIN_WIDTH_PX);
    assert!(DCS_TOP_CONTROL_PIN_WIDTH_PX >= DCS_TOP_CONTROL_MIN_WIDTH_PX);
    assert!(
        DCS_TOOLBAR_SAFE_PADDING_PX >= DCS_TOP_CONTROLS_EXTENT_PX + DCS_TOP_CONTROL_CLEARANCE_PX
    );
    // Strictly greater, not >=: the standard icons must have rounding headroom
    // inside the cap, or a fractional scale factor clips a live control.
    assert!(
        DCS_TOP_CONTROL_SLOT_MAX_WIDTH_PX
            > DCS_TOP_CONTROL_TOGGLE_WIDTH_PX
                + DCS_TOP_CONTROL_GAP_PX
                + DCS_TOP_CONTROL_PIN_WIDTH_PX
    );
    // A clipped oversized control must still stop at the toolbar reservation:
    // the margin widens the clip box, so it is spent from the same budget.
    assert!(
        DCS_TOP_CONTROL_EDGE_OFFSET_PX
            + DCS_TOP_CONTROL_SLOT_MAX_WIDTH_PX
            + DCS_TOP_CONTROL_CLIP_MARGIN_PX
            + DCS_TOP_CONTROL_ROUNDING_ALLOWANCE_PX
            <= DCS_TOOLBAR_SAFE_PADDING_PX
    );
};

use crate::theme::tokens;

/// A shell side.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DcsSide {
    Left,
    Right,
}

/// The effective presentation of a sidebar at the current window width.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DcsSidebarMode {
    Closed,
    Floating,
    Pinned,
}

/// One panel supplied by an application to a sidebar carousel.
pub struct DcsPanel {
    pub id: String,
    pub title: String,
    pub content: Entity,
}

/// Application-supplied visuals for one sidebar's top-bar controls.
///
/// CTK deliberately does not depend on an SVG renderer. Applications can
/// supply SVG, image or other UI entities while the DCS shell owns their
/// layout, interaction and pinned/floating visibility.
pub struct DcsSidebarControlVisuals {
    pub toggle: Entity,
    pub pinned: Entity,
    pub floating: Entity,
}

impl DcsSidebarControlVisuals {
    pub fn new(toggle: Entity, pinned: Entity, floating: Entity) -> Self {
        Self {
            toggle,
            pinned,
            floating,
        }
    }
}

impl DcsPanel {
    pub fn new(id: impl Into<String>, title: impl Into<String>, content: Entity) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            content,
        }
    }
}

/// Initial shell configuration. The toolbar and centre entities remain owned
/// by the application; DCS reparents them into the reusable shell.
pub struct DcsShellProps {
    pub toolbar: Entity,
    pub centre: Entity,
    pub left_panels: Vec<DcsPanel>,
    pub right_panels: Vec<DcsPanel>,
    /// Initial fraction of shell width occupied by the left sidebar.
    pub left_width: f32,
    /// Initial fraction of shell width occupied by the right sidebar.
    pub right_width: f32,
    pub left_open: bool,
    pub right_open: bool,
    pub left_pinned: bool,
    pub right_pinned: bool,
    pub pin_breakpoint: f32,
    /// Optional icon/image visuals for the left top-bar controls.
    pub left_controls: Option<DcsSidebarControlVisuals>,
    /// Optional icon/image visuals for the right top-bar controls.
    pub right_controls: Option<DcsSidebarControlVisuals>,
}

impl DcsShellProps {
    pub fn new(
        toolbar: Entity,
        centre: Entity,
        left_panels: Vec<DcsPanel>,
        right_panels: Vec<DcsPanel>,
    ) -> Self {
        Self {
            toolbar,
            centre,
            left_panels,
            right_panels,
            left_width: 0.15,
            right_width: 0.15,
            left_open: true,
            right_open: true,
            left_pinned: true,
            right_pinned: false,
            pin_breakpoint: 960.0,
            left_controls: None,
            right_controls: None,
        }
    }
}

/// Mutable state for one sidebar. Applications may query and alter it.
#[derive(Clone, Debug)]
pub struct DcsSidebarState {
    pub open: bool,
    pub pin_preference: bool,
    width: f32,
    pub active_panel: usize,
    panel_ids: Vec<String>,
}

impl DcsSidebarState {
    fn new(open: bool, pinned: bool, width: f32, panel_ids: Vec<String>) -> Self {
        Self {
            open,
            pin_preference: open && pinned,
            width: sane_width(width),
            active_panel: 0,
            panel_ids,
        }
    }

    pub fn mode(&self, window_width: f32, pin_breakpoint: f32) -> DcsSidebarMode {
        if !self.open {
            DcsSidebarMode::Closed
        } else if self.pin_preference && window_width >= pin_breakpoint {
            DcsSidebarMode::Pinned
        } else {
            DcsSidebarMode::Floating
        }
    }

    pub fn toggle_open(&mut self) {
        self.open = !self.open;
        if !self.open {
            self.pin_preference = false;
        }
    }

    pub fn toggle_pin(&mut self) {
        self.open = true;
        self.pin_preference = !self.pin_preference;
    }

    pub fn width(&self) -> f32 {
        self.width
    }

    /// Set sidebar width as a fraction of the shell width (`0.10..=1.0`).
    pub fn set_width(&mut self, width: f32) {
        self.width = sane_width(width);
    }

    pub fn previous_panel(&mut self) {
        if !self.panel_ids.is_empty() {
            self.active_panel =
                (self.active_panel + self.panel_ids.len() - 1) % self.panel_ids.len();
        }
    }

    pub fn next_panel(&mut self) {
        if !self.panel_ids.is_empty() {
            self.active_panel = (self.active_panel + 1) % self.panel_ids.len();
        }
    }

    pub fn select_panel(&mut self, index: usize) {
        if index < self.panel_ids.len() {
            self.active_panel = index;
        }
    }

    /// The stable id of the visible carousel panel, suitable for persistence.
    pub fn active_panel_id(&self) -> Option<&str> {
        self.panel_ids.get(self.active_panel).map(String::as_str)
    }

    /// Select a carousel panel using its application-supplied stable id.
    pub fn select_panel_id(&mut self, id: &str) -> bool {
        let Some(index) = self.panel_ids.iter().position(|candidate| candidate == id) else {
            return false;
        };
        self.active_panel = index;
        true
    }
}

fn sane_width(width: f32) -> f32 {
    if width.is_finite() {
        width.clamp(MIN_SIDEBAR_FRACTION, 1.0)
    } else {
        0.15
    }
}

/// Public shell state stored on the shell root.
#[derive(Component, Clone, Debug)]
pub struct DcsShellState {
    pub left: DcsSidebarState,
    pub right: DcsSidebarState,
    pub pin_breakpoint: f32,
}

impl DcsShellState {
    pub fn side(&self, side: DcsSide) -> &DcsSidebarState {
        match side {
            DcsSide::Left => &self.left,
            DcsSide::Right => &self.right,
        }
    }

    pub fn side_mut(&mut self, side: DcsSide) -> &mut DcsSidebarState {
        match side {
            DcsSide::Left => &mut self.left,
            DcsSide::Right => &mut self.right,
        }
    }
}

/// Marker on the root of a DCS shell.
#[derive(Component)]
pub struct DcsShell;

/// Initial content for a reusable two-way centre split.
pub struct DcsSplitProps {
    pub first: Entity,
    pub second: Entity,
    /// Fraction of the available centre width assigned to `first`.
    pub ratio: f32,
}

impl DcsSplitProps {
    pub fn new(first: Entity, second: Entity) -> Self {
        Self {
            first,
            second,
            ratio: 0.5,
        }
    }
}

/// Mutable state for the centre split. Double-clicking the divider resets it.
#[derive(Component, Clone, Copy, Debug)]
pub struct DcsSplitState {
    ratio: f32,
}

impl DcsSplitState {
    pub fn ratio(&self) -> f32 {
        self.ratio
    }

    pub fn set_ratio(&mut self, ratio: f32) {
        self.ratio = sane_split_ratio(ratio);
    }

    pub fn reset(&mut self) {
        self.ratio = 0.5;
    }
}

fn sane_split_ratio(ratio: f32) -> f32 {
    if ratio.is_finite() {
        ratio.clamp(MIN_SPLIT_FRACTION, 1.0 - MIN_SPLIT_FRACTION)
    } else {
        0.5
    }
}

#[derive(Component)]
struct DcsSplitParts {
    first: Entity,
    second: Entity,
}

#[derive(Component)]
struct DcsSplitter {
    split: Entity,
}

#[derive(Component, Clone, Copy)]
struct DcsSidebarResizeHandle {
    shell: Entity,
    side: DcsSide,
}

/// Entities produced by [`spawn_dcs_split`].
pub struct DcsSplitEntities {
    pub root: Entity,
    pub divider: Entity,
}

/// Wrap two application-owned entities in a draggable centre split.
///
/// The divider is constrained so each side retains at least ten percent of
/// the available width. Double-click it to return to an exact 50/50 split.
pub fn spawn_dcs_split(commands: &mut Commands, props: DcsSplitProps) -> DcsSplitEntities {
    let root = commands.spawn_empty().id();
    let first = commands
        .spawn(Node {
            min_width: px(0),
            min_height: px(0),
            flex_basis: px(0),
            flex_grow: sane_split_ratio(props.ratio),
            flex_shrink: 1.0,
            ..default()
        })
        .add_child(props.first)
        .id();
    let second = commands
        .spawn(Node {
            min_width: px(0),
            min_height: px(0),
            flex_basis: px(0),
            flex_grow: 1.0 - sane_split_ratio(props.ratio),
            flex_shrink: 1.0,
            ..default()
        })
        .add_child(props.second)
        .id();
    let mut accessibility = accesskit::Node::new(Role::Splitter);
    accessibility.set_label("Resize file panes; double-click to centre");
    let divider = commands
        .spawn((
            Node {
                width: px(7),
                min_width: px(7),
                height: percent(100),
                ..default()
            },
            ThemeBackgroundColor(tokens::CONTROL),
            Pickable::default(),
            Hovered::default(),
            AccessibilityNode::from(accessibility),
            EntityCursor::System(SystemCursorIcon::EwResize),
            DcsSplitter { split: root },
        ))
        .id();
    commands.entity(root).insert((
        Node {
            width: percent(100),
            min_width: px(0),
            min_height: px(0),
            flex_grow: 1.0,
            flex_direction: FlexDirection::Row,
            ..default()
        },
        DcsSplitState {
            ratio: sane_split_ratio(props.ratio),
        },
        DcsSplitParts { first, second },
    ));
    commands
        .entity(root)
        .add_children(&[first, divider, second]);
    DcsSplitEntities { root, divider }
}

#[derive(Component)]
struct DcsShellParts {
    left: DcsSidebarParts,
    right: DcsSidebarParts,
}

struct DcsSidebarParts {
    sidebar: Entity,
    title: Entity,
    pin_visual: DcsPinVisual,
    panels: Vec<Entity>,
    panel_titles: Vec<String>,
    panel_ids: Vec<String>,
}

enum DcsPinVisual {
    Label(Entity),
    Icons { pinned: Entity, floating: Entity },
}

#[derive(Component, Clone, Copy)]
struct DcsControl {
    shell: Entity,
    side: DcsSide,
    action: DcsAction,
}

#[cfg(debug_assertions)]
#[derive(Component)]
struct DcsTopControlSlot {
    side: DcsSide,
    warned_oversize: bool,
}

#[derive(Clone, Copy)]
enum DcsAction {
    ToggleOpen,
    TogglePin,
    PreviousPanel,
    NextPanel,
}

/// Entities useful to an application after constructing a shell.
pub struct DcsShellEntities {
    pub root: Entity,
    pub top_bar: Entity,
    pub workspace: Entity,
    pub left_sidebar: Entity,
    pub right_sidebar: Entity,
}

/// Spawn a complete DCS shell. The top bar owns open/close and float/pin
/// controls; sidebar headers own carousel navigation. The application supplies
/// all actual panel content and may supply custom control visuals.
pub fn spawn_dcs_shell(commands: &mut Commands, props: DcsShellProps) -> DcsShellEntities {
    let root = commands.spawn_empty().id();
    let (left_toggle_slot, left_pin_visual) = spawn_top_controls(
        commands,
        root,
        DcsSide::Left,
        props.left_controls,
        props.left_pinned,
    );
    let (right_toggle_slot, right_pin_visual) = spawn_top_controls(
        commands,
        root,
        DcsSide::Right,
        props.right_controls,
        props.right_pinned,
    );
    let top_bar = commands
        .spawn((
            Node {
                width: percent(100),
                min_height: px(42),
                flex_direction: FlexDirection::Row,
                align_items: AlignItems::Center,
                justify_content: JustifyContent::Center,
                position_type: PositionType::Relative,
                padding: UiRect::axes(px(7), px(5)),
                ..default()
            },
            ThemeBackgroundColor(tokens::MASTER_PANEL),
        ))
        .add_children(&[left_toggle_slot, props.toolbar, right_toggle_slot])
        .id();

    let (left_sidebar, left_parts) = spawn_sidebar(
        commands,
        root,
        DcsSide::Left,
        props.left_panels,
        left_pin_visual,
    );
    let (right_sidebar, right_parts) = spawn_sidebar(
        commands,
        root,
        DcsSide::Right,
        props.right_panels,
        right_pin_visual,
    );
    let workspace = commands
        .spawn((Node {
            width: percent(100),
            flex_grow: 1.0,
            min_height: px(0),
            flex_direction: FlexDirection::Row,
            position_type: PositionType::Relative,
            overflow: bevy::ui::Overflow::clip(),
            ..default()
        },))
        .add_children(&[left_sidebar, props.centre, right_sidebar])
        .id();

    commands.entity(root).insert((
        Node {
            width: percent(100),
            height: percent(100),
            min_width: px(0),
            min_height: px(0),
            flex_direction: FlexDirection::Column,
            ..default()
        },
        ThemeBackgroundColor(tokens::SURFACE),
        DcsShell,
        DcsShellState {
            left: DcsSidebarState::new(
                props.left_open,
                props.left_pinned,
                props.left_width,
                left_parts.panel_ids.clone(),
            ),
            right: DcsSidebarState::new(
                props.right_open,
                props.right_pinned,
                props.right_width,
                right_parts.panel_ids.clone(),
            ),
            pin_breakpoint: props.pin_breakpoint,
        },
        DcsShellParts {
            left: left_parts,
            right: right_parts,
        },
    ));
    commands.entity(root).add_children(&[top_bar, workspace]);

    DcsShellEntities {
        root,
        top_bar,
        workspace,
        left_sidebar,
        right_sidebar,
    }
}

fn spawn_top_controls(
    commands: &mut Commands,
    shell: Entity,
    side: DcsSide,
    visuals: Option<DcsSidebarControlVisuals>,
    initially_pinned: bool,
) -> (Entity, DcsPinVisual) {
    let (toggle, pin, pin_visual) = if let Some(visuals) = visuals {
        let pinned = commands
            .spawn(Node {
                display: if initially_pinned {
                    Display::Flex
                } else {
                    Display::None
                },
                ..default()
            })
            .add_child(visuals.pinned)
            .id();
        let floating = commands
            .spawn(Node {
                display: if initially_pinned {
                    Display::None
                } else {
                    Display::Flex
                },
                ..default()
            })
            .add_child(visuals.floating)
            .id();
        let toggle = control_button_with_children(
            commands,
            shell,
            side,
            DcsAction::ToggleOpen,
            &[visuals.toggle],
            match side {
                DcsSide::Left => "Toggle left sidebar",
                DcsSide::Right => "Toggle right sidebar",
            },
        );
        let pin = control_button_with_children(
            commands,
            shell,
            side,
            DcsAction::TogglePin,
            &[pinned, floating],
            match side {
                DcsSide::Left => "Toggle left sidebar pinning",
                DcsSide::Right => "Toggle right sidebar pinning",
            },
        );
        (toggle, pin, DcsPinVisual::Icons { pinned, floating })
    } else {
        let toggle = control_button(
            commands,
            shell,
            side,
            DcsAction::ToggleOpen,
            match side {
                DcsSide::Left => "[",
                DcsSide::Right => "]",
            },
            match side {
                DcsSide::Left => "Toggle left sidebar",
                DcsSide::Right => "Toggle right sidebar",
            },
        );
        let (pin, label) = control_button_parts(
            commands,
            shell,
            side,
            DcsAction::TogglePin,
            if initially_pinned { "Float" } else { "Pin" },
            match side {
                DcsSide::Left => "Toggle left sidebar pinning",
                DcsSide::Right => "Toggle right sidebar pinning",
            },
        );
        (toggle, pin, DcsPinVisual::Label(label))
    };

    let controls = match side {
        DcsSide::Left => [toggle, pin],
        DcsSide::Right => [pin, toggle],
    };
    let slot = commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                left: if side == DcsSide::Left {
                    px(DCS_TOP_CONTROL_EDGE_OFFSET_PX)
                } else {
                    bevy::ui::Val::Auto
                },
                right: if side == DcsSide::Right {
                    px(DCS_TOP_CONTROL_EDGE_OFFSET_PX)
                } else {
                    bevy::ui::Val::Auto
                },
                top: px(0),
                bottom: px(0),
                max_width: px(DCS_TOP_CONTROL_SLOT_MAX_WIDTH_PX),
                flex_direction: FlexDirection::Row,
                align_items: AlignItems::Center,
                column_gap: px(DCS_TOP_CONTROL_GAP_PX),
                overflow: bevy::ui::Overflow::clip_x(),
                overflow_clip_margin: bevy::ui::OverflowClipMargin::border_box()
                    .with_margin(DCS_TOP_CONTROL_CLIP_MARGIN_PX),
                ..default()
            },
            GlobalZIndex(FLOAT_Z + 2),
            #[cfg(debug_assertions)]
            DcsTopControlSlot {
                side,
                warned_oversize: false,
            },
        ))
        .add_children(&controls)
        .id();
    (slot, pin_visual)
}

fn spawn_sidebar(
    commands: &mut Commands,
    shell: Entity,
    side: DcsSide,
    panels: Vec<DcsPanel>,
    pin_visual: DcsPinVisual,
) -> (Entity, DcsSidebarParts) {
    let previous = control_button(
        commands,
        shell,
        side,
        DcsAction::PreviousPanel,
        "<",
        "Previous sidebar panel",
    );
    let title = label(
        commands,
        panels.first().map_or("", |panel| &panel.title),
        14.0,
        false,
    );
    let next = control_button(
        commands,
        shell,
        side,
        DcsAction::NextPanel,
        ">",
        "Next sidebar panel",
    );
    let header = commands
        .spawn((Node {
            width: percent(100),
            min_height: px(40),
            flex_direction: FlexDirection::Row,
            align_items: AlignItems::Center,
            column_gap: px(5),
            padding: UiRect::all(px(5)),
            ..default()
        },))
        .add_children(&[previous, title, next])
        .id();

    let mut panel_entities = Vec::with_capacity(panels.len());
    let mut panel_titles = Vec::with_capacity(panels.len());
    let mut panel_ids = Vec::with_capacity(panels.len());
    let panel_host = commands
        .spawn((Node {
            width: percent(100),
            min_width: px(0),
            flex_grow: 1.0,
            min_height: px(0),
            ..default()
        },))
        .id();
    for (index, panel) in panels.into_iter().enumerate() {
        let wrapper = commands
            .spawn(Node {
                width: percent(100),
                min_width: px(0),
                height: percent(100),
                display: if index == 0 {
                    Display::Flex
                } else {
                    Display::None
                },
                ..default()
            })
            .add_child(panel.content)
            .id();
        commands.entity(panel_host).add_child(wrapper);
        panel_entities.push(wrapper);
        panel_titles.push(panel.title);
        panel_ids.push(panel.id);
    }

    let sidebar = commands
        .spawn((
            Node {
                height: percent(100),
                min_width: px(0),
                flex_shrink: 0.0,
                flex_direction: FlexDirection::Column,
                position_type: PositionType::Relative,
                overflow: bevy::ui::Overflow::clip(),
                border: match side {
                    DcsSide::Left => UiRect::right(px(1)),
                    DcsSide::Right => UiRect::left(px(1)),
                },
                ..default()
            },
            ThemeBackgroundColor(tokens::PANEL),
            BorderColor::all(Color::NONE),
            ThemeBorderColor(tokens::BORDER),
            GlobalZIndex(FLOAT_Z),
        ))
        .add_children(&[header, panel_host])
        .id();
    let resize_handle = commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                width: px(7),
                height: percent(100),
                top: px(0),
                left: if side == DcsSide::Right {
                    px(-3)
                } else {
                    bevy::ui::Val::Auto
                },
                right: if side == DcsSide::Left {
                    px(-3)
                } else {
                    bevy::ui::Val::Auto
                },
                ..default()
            },
            BackgroundColor(Color::NONE),
            Pickable::default(),
            Hovered::default(),
            EntityCursor::System(SystemCursorIcon::EwResize),
            DcsSidebarResizeHandle { shell, side },
            GlobalZIndex(FLOAT_Z + 1),
        ))
        .id();
    commands.entity(sidebar).add_child(resize_handle);

    (
        sidebar,
        DcsSidebarParts {
            sidebar,
            title,
            pin_visual,
            panels: panel_entities,
            panel_titles,
            panel_ids,
        },
    )
}

fn control_button(
    commands: &mut Commands,
    shell: Entity,
    side: DcsSide,
    action: DcsAction,
    text: &str,
    accessible_label: &str,
) -> Entity {
    control_button_parts(commands, shell, side, action, text, accessible_label).0
}

fn control_button_parts(
    commands: &mut Commands,
    shell: Entity,
    side: DcsSide,
    action: DcsAction,
    text: &str,
    accessible_label: &str,
) -> (Entity, Entity) {
    let label = label(commands, text, 13.0, false);
    let button =
        control_button_with_children(commands, shell, side, action, &[label], accessible_label);
    (button, label)
}

fn control_button_with_children(
    commands: &mut Commands,
    shell: Entity,
    side: DcsSide,
    action: DcsAction,
    children: &[Entity],
    accessible_label: &str,
) -> Entity {
    let mut accessibility = accesskit::Node::new(Role::Button);
    accessibility.set_label(accessible_label);
    commands
        .spawn((
            Node {
                min_width: px(DCS_TOP_CONTROL_MIN_WIDTH_PX),
                min_height: px(28),
                flex_shrink: 0.0,
                padding: UiRect::axes(px(DCS_TOP_CONTROL_PADDING_X_PX), px(4)),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                border_radius: BorderRadius::all(px(4)),
                ..default()
            },
            ThemeBackgroundColor(tokens::CONTROL),
            Button,
            Pickable::default(),
            Hovered::default(),
            TabIndex(0),
            AccessibilityNode::from(accessibility),
            DcsControl {
                shell,
                side,
                action,
            },
        ))
        .add_children(children)
        .id()
}

fn label(commands: &mut Commands, value: &str, size: f32, dim: bool) -> Entity {
    commands
        .spawn((
            Text::new(value),
            TextFont::from_font_size(size),
            ThemeTextColor(if dim { tokens::TEXT_DIM } else { tokens::TEXT }),
            Node {
                flex_grow: 1.0,
                ..default()
            },
        ))
        .id()
}

/// Installs DCS controls and responsive float/pin layout.
pub struct DcsShellPlugin;

impl Plugin for DcsShellPlugin {
    fn build(&self, app: &mut App) {
        app.add_observer(on_control)
            .add_observer(on_control_clicked)
            .add_observer(on_split_drag)
            .add_observer(on_split_click)
            .add_observer(on_sidebar_drag)
            .add_systems(Update, (sync_shells, sync_splits));
        #[cfg(debug_assertions)]
        app.add_systems(
            PostUpdate,
            warn_oversized_top_control_slots.after(UiSystems::Layout),
        );
    }
}

#[cfg(debug_assertions)]
fn warn_oversized_top_control_slots(
    mut slots: Query<(&mut DcsTopControlSlot, &Children)>,
    computed_nodes: Query<&ComputedNode>,
) {
    for (mut slot, children) in &mut slots {
        let mut control_count = 0_usize;
        let width = children
            .iter()
            .filter_map(|child| computed_nodes.get(*child).ok())
            .map(|computed| {
                control_count += 1;
                computed.unrounded_size().x * computed.inverse_scale_factor()
            })
            .sum::<f32>()
            + DCS_TOP_CONTROL_GAP_PX * control_count.saturating_sub(1) as f32;
        let extent = DCS_TOP_CONTROL_EDGE_OFFSET_PX + width;
        // Warn on what is actually cut off, which is the clip margin's outer
        // edge — demand between the cap and that edge overflows the slot but
        // still renders in full, so calling it clipped would be untrue.
        let visible_extent = DCS_TOP_CONTROL_EDGE_OFFSET_PX
            + DCS_TOP_CONTROL_SLOT_MAX_WIDTH_PX
            + DCS_TOP_CONTROL_CLIP_MARGIN_PX;
        if !slot.warned_oversize && extent > visible_extent {
            warn!(
                "DCS {:?} top controls require {:.1}px from the window edge ({:.1}px content), \
                 exceeding the {:.1}px they can render inside the {:.1}px toolbar reservation; \
                 the controls are clipped",
                slot.side, extent, width, visible_extent, DCS_TOOLBAR_SAFE_PADDING_PX
            );
            slot.warned_oversize = true;
        }
    }
}

fn on_control(
    activated: On<Activate>,
    controls: Query<&DcsControl>,
    mut shells: Query<&mut DcsShellState>,
) {
    apply_control(activated.entity, &controls, &mut shells);
}

fn on_control_clicked(
    mut click: On<Pointer<Click>>,
    controls: Query<&DcsControl>,
    parents: Query<&ChildOf>,
    mut shells: Query<&mut DcsShellState>,
) {
    if click.button != PointerButton::Primary {
        return;
    }
    let mut entity = click.original_event_target();
    if controls.contains(entity) {
        return;
    }
    loop {
        if controls.contains(entity) {
            click.propagate(false);
            apply_control(entity, &controls, &mut shells);
            return;
        }
        let Ok(parent) = parents.get(entity) else {
            return;
        };
        entity = parent.parent();
    }
}

fn apply_control(
    entity: Entity,
    controls: &Query<&DcsControl>,
    shells: &mut Query<&mut DcsShellState>,
) {
    let Ok(control) = controls.get(entity) else {
        return;
    };
    let Ok(mut shell) = shells.get_mut(control.shell) else {
        return;
    };
    let side = shell.side_mut(control.side);
    match control.action {
        DcsAction::ToggleOpen => side.toggle_open(),
        DcsAction::TogglePin => side.toggle_pin(),
        DcsAction::PreviousPanel => side.previous_panel(),
        DcsAction::NextPanel => side.next_panel(),
    }
}

fn on_split_drag(
    mut drag: On<Pointer<Drag>>,
    dividers: Query<&DcsSplitter>,
    mut splits: Query<(&ComputedNode, &mut DcsSplitState)>,
) {
    let Ok(divider) = dividers.get(drag.entity) else {
        return;
    };
    let Ok((computed, mut split)) = splits.get_mut(divider.split) else {
        return;
    };
    let width = computed.size().x;
    if width > 0.0 {
        drag.propagate(false);
        let ratio = split.ratio() + drag.delta.x / width;
        split.set_ratio(ratio);
    }
}

fn on_split_click(
    mut click: On<Pointer<Click>>,
    dividers: Query<&DcsSplitter>,
    mut splits: Query<&mut DcsSplitState>,
) {
    if click.count < 2 {
        return;
    }
    let Ok(divider) = dividers.get(click.entity) else {
        return;
    };
    let Ok(mut split) = splits.get_mut(divider.split) else {
        return;
    };
    click.propagate(false);
    split.reset();
}

fn on_sidebar_drag(
    mut drag: On<Pointer<Drag>>,
    handles: Query<&DcsSidebarResizeHandle>,
    mut shells: Query<(&ComputedNode, &mut DcsShellState)>,
) {
    let Ok(handle) = handles.get(drag.entity) else {
        return;
    };
    let Ok((computed, mut shell)) = shells.get_mut(handle.shell) else {
        return;
    };
    let available = computed.size().x;
    if available <= 0.0 {
        return;
    }
    drag.propagate(false);
    let side = shell.side_mut(handle.side);
    let delta = match handle.side {
        DcsSide::Left => drag.delta.x,
        DcsSide::Right => -drag.delta.x,
    };
    side.set_width(side.width() + delta / available);
}

fn sync_shells(
    windows: Query<&Window, bevy::ecs::query::With<PrimaryWindow>>,
    shells: Query<(&DcsShellState, &DcsShellParts)>,
    mut nodes: Query<&mut Node>,
    mut texts: Query<&mut Text>,
) {
    let window_width = windows.single().map_or(f32::INFINITY, Window::width);
    for (state, parts) in &shells {
        sync_sidebar(
            &state.left,
            state.pin_breakpoint,
            window_width,
            DcsSide::Left,
            &parts.left,
            &mut nodes,
            &mut texts,
        );
        sync_sidebar(
            &state.right,
            state.pin_breakpoint,
            window_width,
            DcsSide::Right,
            &parts.right,
            &mut nodes,
            &mut texts,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn sync_sidebar(
    state: &DcsSidebarState,
    pin_breakpoint: f32,
    window_width: f32,
    side: DcsSide,
    parts: &DcsSidebarParts,
    nodes: &mut Query<&mut Node>,
    texts: &mut Query<&mut Text>,
) {
    let mode = state.mode(window_width, pin_breakpoint);
    if let Ok(mut node) = nodes.get_mut(parts.sidebar) {
        let display = if mode == DcsSidebarMode::Closed {
            Display::None
        } else {
            Display::Flex
        };
        let width = percent(state.width() * 100.0);
        let position_type = if mode == DcsSidebarMode::Floating {
            PositionType::Absolute
        } else {
            PositionType::Relative
        };
        let left = if mode == DcsSidebarMode::Floating && side == DcsSide::Left {
            px(0)
        } else {
            bevy::ui::Val::Auto
        };
        let right = if mode == DcsSidebarMode::Floating && side == DcsSide::Right {
            px(0)
        } else {
            bevy::ui::Val::Auto
        };
        let top = if mode == DcsSidebarMode::Floating {
            px(0)
        } else {
            bevy::ui::Val::Auto
        };
        let bottom = if mode == DcsSidebarMode::Floating {
            px(0)
        } else {
            bevy::ui::Val::Auto
        };
        if node.display != display {
            node.display = display;
        }
        if node.width != width {
            node.width = width;
        }
        if node.min_width != width {
            node.min_width = width;
        }
        if node.max_width != width {
            node.max_width = width;
        }
        if node.position_type != position_type {
            node.position_type = position_type;
        }
        if node.left != left {
            node.left = left;
        }
        if node.right != right {
            node.right = right;
        }
        if node.top != top {
            node.top = top;
        }
        if node.bottom != bottom {
            node.bottom = bottom;
        }
    }
    if let Some(title) = parts.panel_titles.get(state.active_panel) {
        if let Ok(mut text) = texts.get_mut(parts.title) {
            if text.0 != *title {
                text.0.clone_from(title);
            }
        }
    }
    match parts.pin_visual {
        DcsPinVisual::Label(entity) => {
            if let Ok(mut text) = texts.get_mut(entity) {
                let label = if state.pin_preference { "Float" } else { "Pin" };
                if text.0 != label {
                    text.0 = label.into();
                }
            }
        }
        DcsPinVisual::Icons { pinned, floating } => {
            if let Ok(mut node) = nodes.get_mut(pinned) {
                node.display = if state.pin_preference {
                    Display::Flex
                } else {
                    Display::None
                };
            }
            if let Ok(mut node) = nodes.get_mut(floating) {
                node.display = if state.pin_preference {
                    Display::None
                } else {
                    Display::Flex
                };
            }
        }
    }
    for (index, panel) in parts.panels.iter().enumerate() {
        if let Ok(mut node) = nodes.get_mut(*panel) {
            let display = if index == state.active_panel {
                Display::Flex
            } else {
                Display::None
            };
            if node.display != display {
                node.display = display;
            }
        }
    }
}

fn sync_splits(splits: Query<(&DcsSplitState, &DcsSplitParts)>, mut nodes: Query<&mut Node>) {
    for (state, parts) in &splits {
        if let Ok(mut first) = nodes.get_mut(parts.first) {
            first.flex_grow = state.ratio();
        }
        if let Ok(mut second) = nodes.get_mut(parts.second) {
            second.flex_grow = 1.0 - state.ratio();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use bevy::app::PostUpdate;
    use bevy::asset::AssetPlugin;
    use bevy::camera::CameraPlugin;
    use bevy::camera::{Camera, Camera2d, ComputedCameraValues, RenderTargetInfo, Viewport};
    #[cfg(debug_assertions)]
    use bevy::ecs::system::RunSystemOnce;
    use bevy::ecs::world::CommandQueue;
    use bevy::image::{ImagePlugin, TextureAtlasPlugin};
    use bevy::input::InputPlugin;
    use bevy::math::{UVec2, Vec2};
    use bevy::mesh::MeshPlugin;
    use bevy::picking::PickingPlugin;
    use bevy::prelude::MinimalPlugins;
    use bevy::text::TextPlugin;
    use bevy::transform::TransformPlugin;
    use bevy::ui::{CalculatedClip, UiGlobalTransform, UiPlugin};

    #[test]
    fn pin_falls_back_to_float_without_losing_preference() {
        let state = DcsSidebarState::new(true, true, 0.15, vec!["one".into()]);
        assert_eq!(state.mode(1200.0, 960.0), DcsSidebarMode::Pinned);
        assert_eq!(state.mode(800.0, 960.0), DcsSidebarMode::Floating);
        assert!(state.pin_preference);
    }

    #[test]
    fn closing_also_unpins() {
        let mut state = DcsSidebarState::new(true, true, 0.15, vec!["one".into()]);
        state.toggle_open();
        assert_eq!(state.mode(1200.0, 960.0), DcsSidebarMode::Closed);
        assert!(!state.pin_preference);
    }

    #[test]
    fn carousel_wraps_both_ways() {
        let mut state = DcsSidebarState::new(
            true,
            false,
            0.15,
            vec!["one".into(), "two".into(), "three".into()],
        );
        state.previous_panel();
        assert_eq!(state.active_panel, 2);
        state.next_panel();
        assert_eq!(state.active_panel, 0);
        assert!(state.select_panel_id("two"));
        assert_eq!(state.active_panel_id(), Some("two"));
        state.set_width(900.0);
        assert_eq!(state.width(), 1.0);
        state.set_width(f32::NAN);
        assert_eq!(state.width(), 0.15);
    }

    #[test]
    fn sidebar_resize_range_is_ten_to_one_hundred_percent() {
        assert_eq!(sane_width(0.01), 0.1);
        assert_eq!(sane_width(0.25), 0.25);
        assert_eq!(sane_width(1.4), 1.0);
    }

    #[test]
    fn centre_split_clamps_and_resets_exactly() {
        let mut split = DcsSplitState { ratio: 0.5 };
        split.set_ratio(0.01);
        assert_eq!(split.ratio(), 0.1);
        split.set_ratio(0.99);
        assert_eq!(split.ratio(), 0.9);
        split.reset();
        assert_eq!(split.ratio(), 0.5);
    }

    #[test]
    fn two_sides_remain_independent() {
        let mut shell = DcsShellState {
            left: DcsSidebarState::new(true, true, 0.15, vec!["one".into(), "two".into()]),
            right: DcsSidebarState::new(true, false, 0.15, vec!["one".into(), "two".into()]),
            pin_breakpoint: 960.0,
        };
        shell.side_mut(DcsSide::Right).toggle_pin();
        assert!(shell.left.pin_preference);
        assert!(shell.right.pin_preference);
        shell.side_mut(DcsSide::Left).toggle_open();
        assert!(!shell.left.open);
        assert!(shell.right.open);
    }

    fn layout_test_app(scale_factor: f32) -> App {
        const LOGICAL_WIDTH: f32 = 400.0;
        const LOGICAL_HEIGHT: f32 = 100.0;
        let target_width = (LOGICAL_WIDTH * scale_factor) as u32;
        let target_height = (LOGICAL_HEIGHT * scale_factor) as u32;

        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .add_plugins(AssetPlugin::default())
            .add_plugins(TransformPlugin)
            .add_plugins(CameraPlugin)
            .add_plugins(ImagePlugin::default())
            .add_plugins(TextureAtlasPlugin)
            .add_plugins(MeshPlugin)
            .add_plugins(InputPlugin)
            .add_plugins(PickingPlugin)
            .add_plugins(TextPlugin)
            .add_plugins(UiPlugin);
        app.world_mut().spawn((
            Camera2d,
            Camera {
                computed: ComputedCameraValues {
                    target_info: Some(RenderTargetInfo {
                        physical_size: UVec2::new(target_width, target_height),
                        scale_factor,
                    }),
                    ..default()
                },
                viewport: Some(Viewport {
                    physical_size: UVec2::new(target_width, target_height),
                    ..default()
                }),
                ..default()
            },
        ));
        app.finish();
        app.cleanup();
        app
    }

    #[derive(Debug)]
    struct TopControlLayout {
        slot_width: f32,
        clip_min: f32,
        clip_max: f32,
        controls_min: f32,
        controls_max: f32,
        control_widths: [f32; 2],
    }

    fn top_control_layout(
        toggle_width: f32,
        pin_width: f32,
        scale_factor: f32,
    ) -> TopControlLayout {
        let mut app = layout_test_app(scale_factor);
        let toggle = app
            .world_mut()
            .spawn(Node {
                width: px(toggle_width),
                height: px(17),
                flex_shrink: 0.0,
                ..default()
            })
            .id();
        let pinned = app
            .world_mut()
            .spawn(Node {
                width: px(pin_width),
                height: px(17),
                flex_shrink: 0.0,
                ..default()
            })
            .id();
        let floating = app
            .world_mut()
            .spawn(Node {
                width: px(pin_width),
                height: px(17),
                flex_shrink: 0.0,
                ..default()
            })
            .id();
        let shell = app.world_mut().spawn_empty().id();
        let mut queue = CommandQueue::default();
        let slot = {
            let mut commands = Commands::new(&mut queue, app.world());
            spawn_top_controls(
                &mut commands,
                shell,
                DcsSide::Left,
                Some(DcsSidebarControlVisuals::new(toggle, pinned, floating)),
                false,
            )
            .0
        };
        queue.apply(app.world_mut());
        app.world_mut()
            .spawn(Node {
                width: px(400),
                height: px(42),
                ..default()
            })
            .add_child(slot);
        app.world_mut().run_schedule(PostUpdate);

        let world = app.world();
        let controls = world.get::<Children>(slot).unwrap();
        let first = controls[0];
        let second = controls[1];
        let slot_computed = world.get::<ComputedNode>(slot).unwrap();
        let first_computed = world.get::<ComputedNode>(first).unwrap();
        let second_computed = world.get::<ComputedNode>(second).unwrap();
        let first_transform = world.get::<UiGlobalTransform>(first).unwrap();
        let second_transform = world.get::<UiGlobalTransform>(second).unwrap();
        let first_clip = world.get::<CalculatedClip>(first).unwrap().clip;
        let second_clip = world.get::<CalculatedClip>(second).unwrap().clip;

        assert_eq!(first_clip, second_clip);
        TopControlLayout {
            slot_width: slot_computed.size().x,
            clip_min: first_clip.min.x,
            clip_max: first_clip.max.x,
            controls_min: first_transform.translation.x - first_computed.size().x / 2.0,
            controls_max: second_transform.translation.x + second_computed.size().x / 2.0,
            control_widths: [first_computed.size().x, second_computed.size().x],
        }
    }

    #[test]
    fn top_control_slot_clamps_oversize_without_touching_standard_icons() {
        let standard = top_control_layout(
            DCS_TOP_CONTROL_TOGGLE_ICON_PX,
            DCS_TOP_CONTROL_PIN_ICON_PX,
            1.0,
        );
        // The standard icons sit strictly inside the cap rather than exactly on
        // it, so physical-pixel rounding has somewhere to go.
        assert!(standard.slot_width < DCS_TOP_CONTROL_SLOT_MAX_WIDTH_PX);
        assert_eq!(
            DCS_TOP_CONTROL_EDGE_OFFSET_PX + standard.slot_width,
            DCS_TOP_CONTROLS_EXTENT_PX
        );
        assert_eq!(
            standard.control_widths,
            [
                DCS_TOP_CONTROL_TOGGLE_WIDTH_PX,
                DCS_TOP_CONTROL_PIN_WIDTH_PX
            ]
        );
        assert!(standard.controls_min >= standard.clip_min);
        assert!(standard.controls_max <= standard.clip_max);

        let oversized = top_control_layout(100.0, 100.0, 1.0);
        assert_eq!(oversized.slot_width, DCS_TOP_CONTROL_SLOT_MAX_WIDTH_PX);
        assert!(oversized.controls_max > oversized.clip_max);
        assert_eq!(
            oversized.clip_max - oversized.clip_min,
            DCS_TOP_CONTROL_SLOT_MAX_WIDTH_PX + 2.0 * DCS_TOP_CONTROL_CLIP_MARGIN_PX
        );
        // The guarantee that matters: however wide the control, nothing it
        // paints reaches the toolbar content that starts at the reservation.
        assert!(oversized.clip_max <= DCS_TOOLBAR_SAFE_PADDING_PX);
    }

    #[test]
    fn standard_top_controls_survive_fractional_scale_rounding() {
        // The slot's auto width can round down while its children round up, so
        // without the clip margin a fractional scale factor clips a live
        // control. 1.5 was the scale that caught it.
        for scale in [1.0_f32, 1.25, 1.5, 2.0, 2.5] {
            let layout = top_control_layout(
                DCS_TOP_CONTROL_TOGGLE_ICON_PX,
                DCS_TOP_CONTROL_PIN_ICON_PX,
                scale,
            );
            assert!(
                layout.controls_min >= layout.clip_min && layout.controls_max <= layout.clip_max,
                "scale {scale}: {layout:?}"
            );

            // The clip edge and the toolbar boundary round to physical pixels
            // independently, so the oversized path has to be checked in
            // physical units too — a logical-only budget can overshoot.
            let oversized = top_control_layout(100.0, 100.0, scale);
            assert!(
                oversized.clip_max <= DCS_TOOLBAR_SAFE_PADDING_PX * scale,
                "scale {scale}: clip {} exceeds physical reservation {}: {oversized:?}",
                oversized.clip_max,
                DCS_TOOLBAR_SAFE_PADDING_PX * scale
            );
        }
    }

    #[cfg(debug_assertions)]
    #[test]
    fn oversized_top_control_slot_warning_latches_once() {
        let mut world = bevy::ecs::world::World::new();
        let first = world
            .spawn(ComputedNode {
                unrounded_size: Vec2::new(DCS_TOP_CONTROL_TOGGLE_WIDTH_PX, 30.0),
                inverse_scale_factor: 1.0,
                ..default()
            })
            .id();
        let second = world
            .spawn(ComputedNode {
                unrounded_size: Vec2::new(
                    DCS_TOP_CONTROL_SLOT_MAX_WIDTH_PX - DCS_TOP_CONTROL_GAP_PX
                        + DCS_TOP_CONTROL_CLIP_MARGIN_PX
                        - DCS_TOP_CONTROL_TOGGLE_WIDTH_PX
                        + 1.0,
                    30.0,
                ),
                inverse_scale_factor: 1.0,
                ..default()
            })
            .id();
        let slot = world
            .spawn(DcsTopControlSlot {
                side: DcsSide::Left,
                warned_oversize: false,
            })
            .add_children(&[first, second])
            .id();

        world
            .run_system_once(warn_oversized_top_control_slots)
            .unwrap();
        assert!(
            world
                .get::<DcsTopControlSlot>(slot)
                .unwrap()
                .warned_oversize
        );

        world
            .entity_mut(second)
            .get_mut::<ComputedNode>()
            .unwrap()
            .unrounded_size
            .x += 1.0;
        world
            .run_system_once(warn_oversized_top_control_slots)
            .unwrap();
        assert!(
            world
                .get::<DcsTopControlSlot>(slot)
                .unwrap()
                .warned_oversize
        );
    }

    #[cfg(debug_assertions)]
    #[test]
    fn controls_inside_the_clip_margin_do_not_warn() {
        // Demand between the cap and the clip margin's outer edge overflows the
        // slot but still renders in full, so warning that it "is clipped" would
        // be a false report.
        let mut world = bevy::ecs::world::World::new();
        let only = world
            .spawn(ComputedNode {
                unrounded_size: Vec2::new(
                    DCS_TOP_CONTROL_SLOT_MAX_WIDTH_PX + DCS_TOP_CONTROL_CLIP_MARGIN_PX,
                    30.0,
                ),
                inverse_scale_factor: 1.0,
                ..default()
            })
            .id();
        let slot = world
            .spawn(DcsTopControlSlot {
                side: DcsSide::Left,
                warned_oversize: false,
            })
            .add_children(&[only])
            .id();

        world
            .run_system_once(warn_oversized_top_control_slots)
            .unwrap();
        assert!(
            !world
                .get::<DcsTopControlSlot>(slot)
                .unwrap()
                .warned_oversize
        );
    }
}
