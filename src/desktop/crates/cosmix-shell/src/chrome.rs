//! Edge-generic CTK chrome from
//! `_plan/2026-08-06-cosmix-shell-corner-panels.md` §E1, §E2 and §E5.
//!
//! The chrome owns no shell semantics and performs no window queries. A host
//! supplies four mount entities; every visual update is driven only by the
//! renderer-neutral [`ShellFrameState`]. Panels keep their final layout size.
//! The development host lets chrome own the complete off-edge slide, while a
//! layer-shell host selects protocol margins for non-pinned motion and leaves
//! only pinned motion with [`UiTransform::translation`].

use accesskit::Role;
use std::error::Error;
use std::fmt::{Display as FmtDisplay, Formatter};

use bevy::a11y::AccessibilityNode;
use bevy::app::{App, Plugin, Update};
use bevy::ecs::observer::On;
use bevy::ecs::system::SystemParam;
use bevy::input_focus::InputFocus;
use bevy::input_focus::tab_navigation::{TabGroup, TabIndex};
use bevy::picking::Pickable;
use bevy::picking::hover::Hovered;
use bevy::prelude::*;
use bevy::ui::{InteractionDisabled, UiRect, Val2, percent, px};
use bevy::ui_widgets::Activate;
use bevy::window::RequestRedraw;
use ctk::theme::tokens;

use crate::core::{Carousel, CarouselError, Edge, Orientation, PanelInput, PanelMode};
use crate::runtime::{
    CarouselInput, ShellCommand, ShellCommandKind, ShellFrame, ShellFrameState, ShellRuntimeSet,
};

/// The four host-owned attachment points. Chrome assumes nothing about their
/// parents or geometry.
#[derive(Clone, Copy, Debug)]
pub struct QuoinPanelMounts {
    mounts: [Entity; 4],
    motion_ownership: QuoinMotionOwnership,
    pointer_ownership: QuoinPointerOwnership,
}

/// Selects the one visual-motion owner used by chrome for every panel state.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum QuoinMotionOwnership {
    /// Chrome translates every mapped panel (the normal-window development host).
    #[default]
    Chrome,
    /// Protocol margins translate non-pinned panels; chrome translates pinned panels.
    ProtocolWhenUnpinned,
}

/// Selects the source of semantic pointer enter/leave commands.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum QuoinPointerOwnership {
    /// Chrome hover messages own containment in the normal-window dev host.
    #[default]
    ChromeHover,
    /// Native surface events own containment in the layer-shell host.
    NativeSurface,
}

/// Motion modes retained by the host after successful protocol commits.
#[derive(Resource, Clone, Copy, Debug, Eq, PartialEq)]
pub struct QuoinCommittedMotionModes([PanelMode; 4]);

impl QuoinCommittedMotionModes {
    pub const fn hidden() -> Self {
        Self([PanelMode::Hidden; 4])
    }

    pub const fn get(&self, edge: Edge) -> PanelMode {
        self.0[edge.index()]
    }

    pub fn set(&mut self, edge: Edge, mode: PanelMode) {
        self.0[edge.index()] = mode;
    }
}

impl QuoinPanelMounts {
    pub const fn new(left: Entity, bottom: Entity, right: Entity, top: Entity) -> Self {
        let mut mounts = [left; 4];
        mounts[Edge::Bottom.index()] = bottom;
        mounts[Edge::Right.index()] = right;
        mounts[Edge::Top.index()] = top;
        Self {
            mounts,
            motion_ownership: QuoinMotionOwnership::Chrome,
            pointer_ownership: QuoinPointerOwnership::ChromeHover,
        }
    }

    /// Construct mounts for layer-shell surfaces, where protocol margins own
    /// revealed and concealing motion and chrome owns pinned motion.
    pub const fn for_layer_surfaces(
        left: Entity,
        bottom: Entity,
        right: Entity,
        top: Entity,
    ) -> Self {
        let mut mounts = [left; 4];
        mounts[Edge::Bottom.index()] = bottom;
        mounts[Edge::Right.index()] = right;
        mounts[Edge::Top.index()] = top;
        Self {
            mounts,
            motion_ownership: QuoinMotionOwnership::ProtocolWhenUnpinned,
            pointer_ownership: QuoinPointerOwnership::NativeSurface,
        }
    }

    pub const fn get(self, edge: Edge) -> Entity {
        self.mounts[edge.index()]
    }

    pub const fn motion_ownership(self) -> QuoinMotionOwnership {
        self.motion_ownership
    }

    pub const fn pointer_ownership(self) -> QuoinPointerOwnership {
        self.pointer_ownership
    }
}

/// Stable renderer-neutral identity and title for one carousel page.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QuoinPageSpec {
    id: String,
    title: String,
}

impl QuoinPageSpec {
    pub fn new(id: impl Into<String>, title: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
        }
    }
}

/// The one validated page registry shared by model and chrome construction.
#[derive(Resource, Clone, Debug)]
pub struct QuoinPageRegistry {
    panels: [Vec<QuoinPageSpec>; 4],
}

impl QuoinPageRegistry {
    pub fn new(
        left: Vec<QuoinPageSpec>,
        bottom: Vec<QuoinPageSpec>,
        right: Vec<QuoinPageSpec>,
        top: Vec<QuoinPageSpec>,
    ) -> Result<Self, QuoinPageRegistryError> {
        let panels = [left, bottom, right, top];
        for edge in Edge::ALL {
            Carousel::new(panels[edge.index()].iter().map(|page| page.id.as_str()))
                .map_err(|source| QuoinPageRegistryError::InvalidRegistry { edge, source })?;
        }
        Ok(Self { panels })
    }

    /// Model carousel derived from the same validated IDs chrome will bind.
    pub fn carousel(&self, edge: Edge) -> Carousel {
        Carousel::new(
            self.panels[edge.index()]
                .iter()
                .map(|page| page.id.as_str()),
        )
        .expect("QuoinPageRegistry validates IDs at construction")
    }

    /// Validate the actual runtime model snapshot before chrome is spawned.
    pub fn validate_frame(&self, frame: &ShellFrame) -> Result<(), QuoinPageRegistryError> {
        for edge in Edge::ALL {
            let expected = self.panels[edge.index()]
                .iter()
                .map(|page| page.id.clone())
                .collect::<Vec<_>>();
            let actual = &frame.panel(edge).page_ids;
            if actual.as_ref() != expected.as_slice() {
                return Err(QuoinPageRegistryError::ModelMismatch {
                    edge,
                    expected,
                    actual: actual.to_vec(),
                });
            }
        }
        Ok(())
    }

    /// Bind application entities, rejecting any missing, extra or duplicate ID.
    pub fn bind(
        &self,
        frame: &ShellFrame,
        mut bindings: QuoinContentBindings,
    ) -> Result<QuoinChromeProps, QuoinPageRegistryError> {
        self.validate_frame(frame)?;
        let mut panels: [Vec<QuoinPage>; 4] = std::array::from_fn(|_| Vec::new());
        for edge in Edge::ALL {
            let edge_bindings = std::mem::take(&mut bindings.panels[edge.index()]);
            let actual = edge_bindings
                .iter()
                .map(|binding| binding.id.clone())
                .collect::<Vec<_>>();
            Carousel::new(actual.iter().map(String::as_str))
                .map_err(|source| QuoinPageRegistryError::InvalidContent { edge, source })?;
            let expected = self.panels[edge.index()]
                .iter()
                .map(|page| page.id.clone())
                .collect::<Vec<_>>();
            if actual.len() != expected.len() || actual.iter().any(|id| !expected.contains(id)) {
                return Err(QuoinPageRegistryError::ContentMismatch {
                    edge,
                    expected,
                    actual,
                });
            }
            for spec in &self.panels[edge.index()] {
                let binding = edge_bindings
                    .iter()
                    .find(|binding| binding.id == spec.id)
                    .expect("equal validated ID sets contain every page");
                panels[edge.index()].push(QuoinPage {
                    id: spec.id.clone(),
                    title: spec.title.clone(),
                    content: binding.content,
                });
            }
        }
        Ok(QuoinChromeProps { panels })
    }
}

/// One application content entity keyed to a registry page ID.
pub struct QuoinPageContent {
    id: String,
    content: Entity,
}

impl QuoinPageContent {
    pub fn new(id: impl Into<String>, content: Entity) -> Self {
        Self {
            id: id.into(),
            content,
        }
    }
}

/// Entity bindings for all four panels. Validation occurs in registry order.
#[derive(Default)]
pub struct QuoinContentBindings {
    panels: [Vec<QuoinPageContent>; 4],
}

impl QuoinContentBindings {
    pub fn set(&mut self, edge: Edge, pages: Vec<QuoinPageContent>) {
        self.panels[edge.index()] = pages;
    }
}

/// Invalid page identity or disagreement between model registry and chrome.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum QuoinPageRegistryError {
    InvalidRegistry {
        edge: Edge,
        source: CarouselError,
    },
    InvalidContent {
        edge: Edge,
        source: CarouselError,
    },
    ModelMismatch {
        edge: Edge,
        expected: Vec<String>,
        actual: Vec<String>,
    },
    ContentMismatch {
        edge: Edge,
        expected: Vec<String>,
        actual: Vec<String>,
    },
}

impl FmtDisplay for QuoinPageRegistryError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidRegistry { edge, source } => {
                write!(formatter, "invalid {edge:?} page registry: {source}")
            }
            Self::InvalidContent { edge, source } => {
                write!(formatter, "invalid {edge:?} content bindings: {source}")
            }
            Self::ModelMismatch {
                edge,
                expected,
                actual,
            } => write!(
                formatter,
                "{edge:?} model carousel disagrees with registry: expected {expected:?}, got {actual:?}"
            ),
            Self::ContentMismatch {
                edge,
                expected,
                actual,
            } => write!(
                formatter,
                "{edge:?} content IDs disagree with registry: expected {expected:?}, got {actual:?}"
            ),
        }
    }
}

impl Error for QuoinPageRegistryError {}

#[derive(Debug)]
struct QuoinPage {
    id: String,
    title: String,
    content: Entity,
}

/// Validated pages for all four edge-generic panel instances.
#[derive(Debug)]
pub struct QuoinChromeProps {
    panels: [Vec<QuoinPage>; 4],
}

/// Marker for clock text reproduced from [`crate::runtime::ShellFrame`].
#[derive(Component)]
pub struct QuoinClock;

#[derive(Component)]
struct QuoinPanelChrome {
    edge: Edge,
    motion_ownership: QuoinMotionOwnership,
    pointer_ownership: QuoinPointerOwnership,
}

#[derive(Component)]
struct QuoinPanelParts {
    pin_label: Entity,
    title_label: Entity,
    page_titles: Vec<(String, String)>,
    page_wrappers: Vec<(String, Entity)>,
    dot_labels: Vec<(String, Entity)>,
    controls: Vec<Entity>,
}

#[derive(SystemParam)]
struct PresentPanelQueries<'w, 's> {
    panels: Query<
        'w,
        's,
        (
            &'static QuoinPanelChrome,
            &'static QuoinPanelParts,
            &'static mut Node,
            &'static mut UiTransform,
        ),
    >,
    nodes: Query<'w, 's, &'static mut Node, Without<QuoinPanelChrome>>,
    labels: Query<'w, 's, &'static mut Text>,
    tab_indices: Query<'w, 's, &'static mut TabIndex>,
    disabled_controls: Query<'w, 's, Has<InteractionDisabled>>,
}

#[derive(Component, Clone)]
struct QuoinControl {
    edge: Edge,
    action: QuoinAction,
}

#[derive(Clone)]
enum QuoinAction {
    TogglePin,
    Previous,
    Next,
    Select(String),
}

/// Installs semantic chrome input and ShellFrame-only reconciliation.
pub struct QuoinChromePlugin;

impl Plugin for QuoinChromePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<InputFocus>()
            .add_observer(on_activate)
            .add_systems(
                Update,
                (panel_hover, escape_panels)
                    .chain()
                    .in_set(ShellRuntimeSet::Input),
            )
            .add_systems(
                Update,
                (present_panels, present_content)
                    .chain()
                    .in_set(ShellRuntimeSet::Presentation),
            );
    }
}

/// Construct bottom first, then left/right/top from the identical component.
pub fn spawn_quoin_chrome(
    commands: &mut Commands,
    mounts: QuoinPanelMounts,
    props: QuoinChromeProps,
) {
    let mut panels = props.panels;
    for edge in [Edge::Bottom, Edge::Left, Edge::Right, Edge::Top] {
        spawn_panel(
            commands,
            mounts.get(edge),
            edge,
            mounts.motion_ownership(),
            mounts.pointer_ownership(),
            std::mem::take(&mut panels[edge.index()]),
        );
    }
}

fn spawn_panel(
    commands: &mut Commands,
    mount: Entity,
    edge: Edge,
    motion_ownership: QuoinMotionOwnership,
    pointer_ownership: QuoinPointerOwnership,
    pages: Vec<QuoinPage>,
) {
    let page_titles = pages
        .iter()
        .map(|page| (page.id.clone(), page.title.clone()))
        .collect::<Vec<_>>();
    let pin_label = text(commands, "◇", 15.0, false);
    let pin = button(
        commands,
        edge,
        QuoinAction::TogglePin,
        pin_label,
        "Pin panel",
    );
    let previous_label = text(commands, "‹", 17.0, false);
    let previous = button(
        commands,
        edge,
        QuoinAction::Previous,
        previous_label,
        "Previous page",
    );
    let next_label = text(commands, "›", 17.0, false);
    let next = button(commands, edge, QuoinAction::Next, next_label, "Next page");
    let title_label = text(
        commands,
        page_titles
            .first()
            .map(|(_, title)| title.as_str())
            .unwrap_or("Panel"),
        12.0,
        false,
    );

    let dots = commands
        .spawn(Node {
            flex_direction: FlexDirection::Row,
            align_items: AlignItems::Center,
            column_gap: px(3),
            ..default()
        })
        .id();
    let mut controls = vec![pin, previous, next];
    let mut dot_labels = Vec::with_capacity(pages.len());
    for page in &pages {
        let label = text(commands, "○", 11.0, true);
        let dot = button(
            commands,
            edge,
            QuoinAction::Select(page.id.clone()),
            label,
            &format!("Show {}", page.title),
        );
        commands.entity(dots).add_child(dot);
        controls.push(dot);
        dot_labels.push((page.id.clone(), label));
    }

    let header = commands
        .spawn((
            Node {
                min_width: px(0),
                min_height: px(34),
                flex_shrink: 0.0,
                flex_direction: FlexDirection::Row,
                align_items: AlignItems::Center,
                justify_content: JustifyContent::Center,
                column_gap: px(4),
                padding: UiRect::axes(px(5), px(3)),
                ..default()
            },
            bevy::feathers::theme::ThemeBackgroundColor(tokens::MASTER_PANEL),
        ))
        .add_children(&[pin, title_label, previous, dots, next])
        .id();

    let page_host = commands
        .spawn(Node {
            min_width: px(0),
            min_height: px(0),
            flex_grow: 1.0,
            overflow: Overflow::clip(),
            ..default()
        })
        .id();
    let mut page_wrappers = Vec::with_capacity(pages.len());
    for (index, page) in pages.into_iter().enumerate() {
        let wrapper = commands
            .spawn(Node {
                width: percent(100),
                height: percent(100),
                display: if index == 0 {
                    Display::Flex
                } else {
                    Display::None
                },
                ..default()
            })
            .add_child(page.content)
            .id();
        commands.entity(page_host).add_child(wrapper);
        page_wrappers.push((page.id, wrapper));
    }

    let horizontal = edge.orientation() == Orientation::Horizontal;
    let root = commands
        .spawn((
            Node {
                width: percent(100),
                height: percent(100),
                min_width: px(0),
                min_height: px(0),
                flex_direction: if horizontal {
                    FlexDirection::Row
                } else {
                    FlexDirection::Column
                },
                border: panel_border(edge),
                ..default()
            },
            UiTransform::default(),
            bevy::feathers::theme::ThemeBackgroundColor(tokens::PANEL),
            bevy::feathers::theme::ThemeBorderColor(tokens::BORDER),
            BorderColor::all(Color::NONE),
            Pickable::default(),
            Hovered::default(),
            TabGroup::new(edge.index() as i32),
            QuoinPanelChrome {
                edge,
                motion_ownership,
                pointer_ownership,
            },
            QuoinPanelParts {
                pin_label,
                title_label,
                page_titles,
                page_wrappers,
                dot_labels,
                controls,
            },
        ))
        .add_children(&[header, page_host])
        .id();
    commands.entity(mount).add_child(root);
}

fn panel_border(edge: Edge) -> UiRect {
    match edge {
        Edge::Left => UiRect::right(px(1)),
        Edge::Bottom => UiRect::top(px(1)),
        Edge::Right => UiRect::left(px(1)),
        Edge::Top => UiRect::bottom(px(1)),
    }
}

fn button(
    commands: &mut Commands,
    edge: Edge,
    action: QuoinAction,
    label: Entity,
    accessible_label: &str,
) -> Entity {
    let mut accessibility = accesskit::Node::new(Role::Button);
    accessibility.set_label(accessible_label);
    commands
        .spawn((
            Node {
                min_width: px(28),
                min_height: px(26),
                padding: UiRect::axes(px(6), px(3)),
                align_items: AlignItems::Center,
                justify_content: JustifyContent::Center,
                border_radius: BorderRadius::all(px(4)),
                ..default()
            },
            bevy::feathers::theme::ThemeBackgroundColor(tokens::CONTROL),
            Button,
            Pickable::default(),
            Hovered::default(),
            // Panels start unmapped; ShellFrame reconciliation opts controls
            // into navigation only while their panel surface is mapped.
            TabIndex(-1),
            InteractionDisabled,
            AccessibilityNode::from(accessibility),
            QuoinControl { edge, action },
        ))
        .add_child(label)
        .id()
}

fn text(commands: &mut Commands, value: &str, size: f32, dim: bool) -> Entity {
    commands
        .spawn((
            Text::new(value),
            TextFont::from_font_size(size),
            bevy::feathers::theme::ThemeTextColor(if dim {
                tokens::TEXT_DIM
            } else {
                tokens::TEXT
            }),
            Pickable::IGNORE,
        ))
        .id()
}

fn on_activate(
    activated: On<Activate>,
    controls: Query<(&QuoinControl, Has<InteractionDisabled>)>,
    frame: Res<ShellFrameState>,
    time: Res<Time<Real>>,
    mut commands: MessageWriter<ShellCommand>,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    let Ok((control, disabled)) = controls.get(activated.entity) else {
        return;
    };
    if disabled || !frame.0.panel(control.edge).mapped {
        return;
    }
    let kind = match &control.action {
        QuoinAction::TogglePin => ShellCommandKind::Panel {
            edge: control.edge,
            input: if frame.0.panel(control.edge).mode == PanelMode::Pinned {
                PanelInput::Unpin
            } else {
                PanelInput::Pin
            },
        },
        QuoinAction::Previous => ShellCommandKind::Carousel {
            edge: control.edge,
            input: CarouselInput::Previous,
        },
        QuoinAction::Next => ShellCommandKind::Carousel {
            edge: control.edge,
            input: CarouselInput::Next,
        },
        QuoinAction::Select(id) => ShellCommandKind::Carousel {
            edge: control.edge,
            input: CarouselInput::SelectId(id.clone()),
        },
    };
    commands.write(ShellCommand {
        output: frame.0.geometry.output.clone(),
        at: time.elapsed(),
        kind,
    });
    // Activate is produced by the widget layer during Update and may occur
    // after the model set. Guarantee one follow-up pass in reactive mode.
    redraw.write(RequestRedraw);
}

fn panel_hover(
    changed: Query<(&QuoinPanelChrome, &Hovered), Changed<Hovered>>,
    frame: Res<ShellFrameState>,
    time: Res<Time<Real>>,
    mut commands: MessageWriter<ShellCommand>,
) {
    for (panel, hovered) in &changed {
        if panel.pointer_ownership == QuoinPointerOwnership::NativeSurface {
            continue;
        }
        commands.write(ShellCommand {
            output: frame.0.geometry.output.clone(),
            at: time.elapsed(),
            kind: ShellCommandKind::Panel {
                edge: panel.edge,
                input: if hovered.0 {
                    PanelInput::PointerEntered
                } else {
                    PanelInput::PointerLeft
                },
            },
        });
    }
}

fn escape_panels(
    keys: Res<ButtonInput<KeyCode>>,
    frame: Res<ShellFrameState>,
    time: Res<Time<Real>>,
    mut commands: MessageWriter<ShellCommand>,
) {
    if !keys.just_pressed(KeyCode::Escape) {
        return;
    }
    for edge in Edge::ALL {
        if frame.0.panel(edge).mapped {
            commands.write(ShellCommand {
                output: frame.0.geometry.output.clone(),
                at: time.elapsed(),
                kind: ShellCommandKind::Panel {
                    edge,
                    input: PanelInput::Escape,
                },
            });
        }
    }
}

fn present_panels(
    mut commands: Commands,
    frame: Res<ShellFrameState>,
    committed_modes: Option<Res<QuoinCommittedMotionModes>>,
    mut focus: ResMut<InputFocus>,
    mut queries: PresentPanelQueries,
) {
    for (chrome, parts, mut node, mut transform) in &mut queries.panels {
        let panel = frame.0.panel(chrome.edge);
        node.display = if panel.mapped {
            Display::Flex
        } else {
            Display::None
        };
        for control in &parts.controls {
            if let Ok(mut tab_index) = queries.tab_indices.get_mut(*control) {
                tab_index.0 = if panel.mapped { 0 } else { -1 };
            }
            let disabled = queries.disabled_controls.get(*control).unwrap_or(false);
            if panel.mapped && disabled {
                commands.entity(*control).remove::<InteractionDisabled>();
            } else if !panel.mapped && !disabled {
                commands.entity(*control).insert(InteractionDisabled);
            }
            if !panel.mapped && focus.get() == Some(*control) {
                focus.clear();
            }
        }
        let chrome_owns_motion = match chrome.motion_ownership {
            QuoinMotionOwnership::Chrome => true,
            QuoinMotionOwnership::ProtocolWhenUnpinned => {
                committed_modes
                    .as_ref()
                    .expect("layer mounts require QuoinCommittedMotionModes")
                    .get(chrome.edge)
                    == PanelMode::Pinned
            }
        };
        let hidden = if chrome_owns_motion {
            (1.0 - panel.visible_fraction) * panel.thickness_px
        } else {
            0.0
        };
        transform.translation = match chrome.edge {
            Edge::Left => Val2::new(px(-hidden), px(0)),
            Edge::Bottom => Val2::new(px(0), px(hidden)),
            Edge::Right => Val2::new(px(hidden), px(0)),
            Edge::Top => Val2::new(px(0), px(-hidden)),
        };
        if let Ok(mut label) = queries.labels.get_mut(parts.pin_label) {
            label.0 = if panel.mode == PanelMode::Pinned {
                "◆".to_owned()
            } else {
                "◇".to_owned()
            };
        }
        if let Some(title) = panel.active_page_id.as_deref().and_then(|active| {
            parts
                .page_titles
                .iter()
                .find_map(|(id, title)| (id == active).then_some(title))
        }) && let Ok(mut label) = queries.labels.get_mut(parts.title_label)
        {
            label.0.clone_from(title);
        }
        for (id, entity) in &parts.page_wrappers {
            if let Ok(mut page_node) = queries.nodes.get_mut(*entity) {
                page_node.display = if panel.active_page_id.as_deref() == Some(id) {
                    Display::Flex
                } else {
                    Display::None
                };
            }
        }
        for (id, entity) in &parts.dot_labels {
            if let Ok(mut label) = queries.labels.get_mut(*entity) {
                label.0 = if panel.active_page_id.as_deref() == Some(id) {
                    "●".to_owned()
                } else {
                    "○".to_owned()
                };
            }
        }
    }
}

fn present_content(frame: Res<ShellFrameState>, mut clocks: Query<&mut Text, With<QuoinClock>>) {
    let Some(value) = &frame.0.content.bottom_clock_text else {
        return;
    };
    for mut clock in &mut clocks {
        clock.0.clone_from(value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{LogicalSize, OutputKey, PanelInput, ShellModel};
    use bevy::camera::RenderTarget;
    use bevy::ecs::message::Messages;
    use bevy::ecs::system::RunSystemOnce;
    use bevy::input::ButtonState;
    use bevy::input::mouse::{MouseButton, MouseButtonInput};
    use bevy::picking::PickingSettings;
    use bevy::picking::backend::HitData;
    use bevy::picking::events::{
        Cancel, Click, Drag, DragDrop, DragEnd, DragEnter, DragLeave, DragOver, DragStart, Enter,
        Leave, Move, Out, Over, Pointer, PointerState, Press, Release, Scroll, pointer_events,
    };
    use bevy::picking::hover::{HoverMap, PreviousHoverMap};
    use bevy::picking::input::mouse_pick_events;
    use bevy::picking::pointer::{
        Location, PointerId, PointerInput, PointerLocation, PointerMap, PointerPress,
        update_pointer_map,
    };
    use bevy::ui_widgets::ButtonPlugin;
    use bevy::window::{CursorMoved, WindowEvent, WindowRef};
    use std::time::Duration;

    fn spec(id: &str) -> QuoinPageSpec {
        QuoinPageSpec::new(id, id)
    }

    fn frame_for(registry: &QuoinPageRegistry) -> ShellFrame {
        let mut model = ShellModel::new(
            OutputKey::new("test").unwrap(),
            LogicalSize::new(1_000.0, 800.0).unwrap(),
            Duration::ZERO,
            Duration::from_millis(300),
            Duration::from_millis(180),
        )
        .unwrap();
        for edge in Edge::ALL {
            model.set_carousel(edge, registry.carousel(edge));
        }
        ShellFrame::from_model(&model)
    }

    fn pointer_click_command(action: QuoinAction) -> ShellCommandKind {
        let output = OutputKey::new("test").unwrap();
        let mut model = ShellModel::new(
            output,
            LogicalSize::new(1_000.0, 800.0).unwrap(),
            Duration::ZERO,
            Duration::from_millis(300),
            Duration::from_millis(180),
        )
        .unwrap();
        model
            .panel_input(Edge::Left, Duration::ZERO, PanelInput::Reveal)
            .unwrap();

        let mut app = App::new();
        app.add_plugins((ButtonPlugin, QuoinChromePlugin))
            .insert_resource(Time::<Real>::default())
            .insert_resource(ButtonInput::<KeyCode>::default())
            .insert_resource(ShellFrameState(ShellFrame::from_model(&model)))
            .init_resource::<PickingSettings>()
            .init_resource::<PointerState>()
            .init_resource::<PointerMap>()
            .init_resource::<HoverMap>()
            .init_resource::<PreviousHoverMap>()
            .add_message::<PointerInput>()
            .add_message::<Pointer<Cancel>>()
            .add_message::<Pointer<Click>>()
            .add_message::<Pointer<Press>>()
            .add_message::<Pointer<DragDrop>>()
            .add_message::<Pointer<DragEnd>>()
            .add_message::<Pointer<DragEnter>>()
            .add_message::<Pointer<Drag>>()
            .add_message::<Pointer<DragLeave>>()
            .add_message::<Pointer<DragOver>>()
            .add_message::<Pointer<DragStart>>()
            .add_message::<Pointer<Scroll>>()
            .add_message::<Pointer<Move>>()
            .add_message::<Pointer<Out>>()
            .add_message::<Pointer<Over>>()
            .add_message::<Pointer<Leave>>()
            .add_message::<Pointer<Enter>>()
            .add_message::<Pointer<Release>>()
            .add_message::<WindowEvent>()
            .add_message::<ShellCommand>()
            .add_message::<RequestRedraw>();
        app.finish();
        app.cleanup();
        let window = app.world_mut().spawn(Window::default()).id();
        let target = RenderTarget::Window(WindowRef::Entity(window))
            .normalize(None)
            .unwrap();
        let location = Location {
            target: target.clone(),
            position: Vec2::new(12.0, 8.0),
        };
        app.world_mut().spawn((
            PointerId::Mouse,
            PointerLocation::new(location),
            PointerPress::default(),
        ));
        app.world_mut()
            .run_system_cached(update_pointer_map)
            .unwrap();
        assert!(
            app.world()
                .resource::<PointerMap>()
                .get_entity(PointerId::Mouse)
                .is_some()
        );
        let control = app
            .world_mut()
            .spawn((
                bevy::ui_widgets::Button,
                QuoinControl {
                    edge: Edge::Left,
                    action,
                },
            ))
            .id();
        let hit = HitData::new(control, 0.0, None, None);
        app.world_mut()
            .resource_mut::<HoverMap>()
            .entry(PointerId::Mouse)
            .or_default()
            .insert(control, hit.clone());
        app.world_mut()
            .resource_mut::<PreviousHoverMap>()
            .entry(PointerId::Mouse)
            .or_default()
            .insert(control, hit);

        app.world_mut()
            .write_message(WindowEvent::CursorMoved(CursorMoved {
                window,
                position: Vec2::new(12.0, 8.0),
                delta: None,
            }));
        app.world_mut()
            .write_message(WindowEvent::MouseButtonInput(MouseButtonInput {
                button: MouseButton::Left,
                state: ButtonState::Pressed,
                window,
            }));
        app.world_mut()
            .run_system_cached(mouse_pick_events)
            .unwrap();
        assert!(
            app.world().resource::<Messages<PointerInput>>().len() >= 2,
            "cursor and press must translate into pointer input"
        );
        app.world_mut()
            .run_system_cached(PointerInput::receive)
            .unwrap();
        app.world_mut().run_system_cached(pointer_events).unwrap();
        app.world_mut().flush();
        assert!(
            !app.world()
                .resource::<Messages<Pointer<Press>>>()
                .is_empty(),
            "pointer synthesis must target the control"
        );
        assert!(
            app.world().entity(control).contains::<bevy::ui::Pressed>(),
            "press must traverse Bevy picking into the Button observer"
        );
        app.world_mut()
            .write_message(WindowEvent::MouseButtonInput(MouseButtonInput {
                button: MouseButton::Left,
                state: ButtonState::Released,
                window,
            }));
        app.world_mut()
            .run_system_cached(mouse_pick_events)
            .unwrap();
        app.world_mut()
            .run_system_cached(PointerInput::receive)
            .unwrap();
        app.world_mut().run_system_cached(pointer_events).unwrap();
        app.world_mut().flush();
        assert_eq!(app.world().resource::<Messages<Pointer<Click>>>().len(), 1);
        assert_eq!(
            app.world().resource::<Messages<RequestRedraw>>().len(),
            1,
            "a pointer activation must request the non-Winit host follow-up"
        );
        app.world_mut()
            .resource_mut::<Messages<ShellCommand>>()
            .drain()
            .next()
            .expect("pointer click activates the chrome control")
            .kind
    }

    #[test]
    fn pointer_cursor_press_release_activates_pin_both_chevrons_and_dot() {
        assert_eq!(
            pointer_click_command(QuoinAction::TogglePin),
            ShellCommandKind::Panel {
                edge: Edge::Left,
                input: PanelInput::Pin,
            }
        );
        assert_eq!(
            pointer_click_command(QuoinAction::Previous),
            ShellCommandKind::Carousel {
                edge: Edge::Left,
                input: CarouselInput::Previous,
            }
        );
        assert_eq!(
            pointer_click_command(QuoinAction::Next),
            ShellCommandKind::Carousel {
                edge: Edge::Left,
                input: CarouselInput::Next,
            }
        );
        assert_eq!(
            pointer_click_command(QuoinAction::Select("places".to_owned())),
            ShellCommandKind::Carousel {
                edge: Edge::Left,
                input: CarouselInput::SelectId("places".to_owned()),
            }
        );
    }

    #[test]
    fn registry_rejects_duplicate_model_ids() {
        assert!(matches!(
            QuoinPageRegistry::new(
                vec![spec("same"), spec("same")],
                Vec::new(),
                Vec::new(),
                Vec::new(),
            ),
            Err(QuoinPageRegistryError::InvalidRegistry {
                edge: Edge::Left,
                source: CarouselError::DuplicateId(id),
            }) if id == "same"
        ));
    }

    #[test]
    fn registry_rejects_chrome_content_mismatch() {
        let registry = QuoinPageRegistry::new(
            vec![spec("nav"), spec("places")],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )
        .unwrap();
        let mut bindings = QuoinContentBindings::default();
        bindings.set(
            Edge::Left,
            vec![QuoinPageContent::new("nav", Entity::PLACEHOLDER)],
        );
        let frame = frame_for(&registry);
        assert_eq!(
            registry.bind(&frame, bindings).unwrap_err(),
            QuoinPageRegistryError::ContentMismatch {
                edge: Edge::Left,
                expected: vec!["nav".to_owned(), "places".to_owned()],
                actual: vec!["nav".to_owned()],
            }
        );
    }

    #[test]
    fn registry_rejects_runtime_model_carousel_mismatch() {
        let registry =
            QuoinPageRegistry::new(vec![spec("nav")], Vec::new(), Vec::new(), Vec::new()).unwrap();
        let mut model = ShellModel::new(
            OutputKey::new("test").unwrap(),
            LogicalSize::new(1_000.0, 800.0).unwrap(),
            Duration::ZERO,
            Duration::from_millis(300),
            Duration::from_millis(180),
        )
        .unwrap();
        model.set_carousel(Edge::Left, Carousel::new(["other"]).unwrap());
        let frame = ShellFrame::from_model(&model);

        assert_eq!(
            registry.validate_frame(&frame).unwrap_err(),
            QuoinPageRegistryError::ModelMismatch {
                edge: Edge::Left,
                expected: vec!["nav".to_owned()],
                actual: vec!["other".to_owned()],
            }
        );
    }

    #[test]
    fn layer_mounts_leave_unpinned_motion_to_protocol_but_own_pinned_motion() {
        let mut model = ShellModel::new(
            OutputKey::new("test").unwrap(),
            LogicalSize::new(1_000.0, 800.0).unwrap(),
            Duration::ZERO,
            Duration::from_millis(300),
            Duration::from_millis(180),
        )
        .unwrap();
        model
            .panel_input(Edge::Left, Duration::ZERO, PanelInput::Reveal)
            .unwrap();

        let mut world = World::new();
        let pin_label = world.spawn(Text::new("◇")).id();
        let title_label = world.spawn(Text::new("Panel")).id();
        let chrome = world
            .spawn((
                QuoinPanelChrome {
                    edge: Edge::Left,
                    motion_ownership: QuoinMotionOwnership::ProtocolWhenUnpinned,
                    pointer_ownership: QuoinPointerOwnership::NativeSurface,
                },
                QuoinPanelParts {
                    pin_label,
                    title_label,
                    page_titles: Vec::new(),
                    page_wrappers: Vec::new(),
                    dot_labels: Vec::new(),
                    controls: Vec::new(),
                },
                Node::default(),
                UiTransform::default(),
            ))
            .id();
        world.insert_resource(ShellFrameState(ShellFrame::from_model(&model)));
        world.insert_resource(QuoinCommittedMotionModes::hidden());
        world.insert_resource(InputFocus::default());
        world.run_system_once(present_panels).unwrap();
        assert_eq!(
            world.get::<UiTransform>(chrome).unwrap().translation,
            Val2::new(px(0), px(0))
        );

        model
            .panel_input(Edge::Left, Duration::ZERO, PanelInput::Pin)
            .unwrap();
        world.resource_mut::<ShellFrameState>().0 = ShellFrame::from_model(&model);
        world.run_system_once(present_panels).unwrap();
        // Current pinned with committed unpinned remains protocol-owned.
        assert_eq!(
            world.get::<UiTransform>(chrome).unwrap().translation,
            Val2::new(px(0), px(0))
        );
        world
            .resource_mut::<QuoinCommittedMotionModes>()
            .set(Edge::Left, PanelMode::Pinned);
        world.run_system_once(present_panels).unwrap();
        assert_eq!(
            world.get::<UiTransform>(chrome).unwrap().translation,
            Val2::new(px(-240.0), px(0))
        );
        model
            .panel_input(Edge::Left, Duration::ZERO, PanelInput::Unpin)
            .unwrap();
        world.resource_mut::<ShellFrameState>().0 = ShellFrame::from_model(&model);
        world.run_system_once(present_panels).unwrap();
        // Current unpinned with committed pinned remains chrome-owned.
        assert_eq!(
            world.get::<UiTransform>(chrome).unwrap().translation,
            Val2::new(px(-240.0), px(0))
        );
        world
            .resource_mut::<QuoinCommittedMotionModes>()
            .set(Edge::Left, PanelMode::Revealed);
        world.run_system_once(present_panels).unwrap();
        assert_eq!(
            world.get::<UiTransform>(chrome).unwrap().translation,
            Val2::new(px(0), px(0))
        );
        assert_eq!(model.panel(Edge::Left).visible_fraction, 0.0);
        assert_eq!(model.panel(Edge::Left).exclusive_zone_px, 0.0);
    }

    #[test]
    fn unmapping_panel_disables_controls_and_clears_focus() {
        let mut model = ShellModel::new(
            OutputKey::new("test").unwrap(),
            LogicalSize::new(1_000.0, 800.0).unwrap(),
            Duration::ZERO,
            Duration::from_millis(300),
            Duration::from_millis(180),
        )
        .unwrap();
        model
            .panel_input(Edge::Left, Duration::ZERO, PanelInput::Reveal)
            .unwrap();
        let mapped = ShellFrame::from_model(&model);

        let mut world = World::new();
        let control = world.spawn((TabIndex(-1), InteractionDisabled)).id();
        let pin_label = world.spawn(Text::new("◇")).id();
        let title_label = world.spawn(Text::new("Panel")).id();
        world.spawn((
            QuoinPanelChrome {
                edge: Edge::Left,
                motion_ownership: QuoinMotionOwnership::Chrome,
                pointer_ownership: QuoinPointerOwnership::ChromeHover,
            },
            QuoinPanelParts {
                pin_label,
                title_label,
                page_titles: Vec::new(),
                page_wrappers: Vec::new(),
                dot_labels: Vec::new(),
                controls: vec![control],
            },
            Node::default(),
            UiTransform::default(),
        ));
        world.insert_resource(ShellFrameState(mapped));
        world.insert_resource(InputFocus::default());
        world.run_system_once(present_panels).unwrap();
        assert_eq!(world.get::<TabIndex>(control), Some(&TabIndex(0)));
        assert!(!world.entity(control).contains::<InteractionDisabled>());

        let hidden_model = ShellModel::new(
            OutputKey::new("test").unwrap(),
            LogicalSize::new(1_000.0, 800.0).unwrap(),
            Duration::ZERO,
            Duration::from_millis(300),
            Duration::from_millis(180),
        )
        .unwrap();
        world.resource_mut::<ShellFrameState>().0 = ShellFrame::from_model(&hidden_model);
        *world.resource_mut::<InputFocus>() = InputFocus::from_entity(control);
        world.run_system_once(present_panels).unwrap();
        assert_eq!(world.get::<TabIndex>(control), Some(&TabIndex(-1)));
        assert!(world.entity(control).contains::<InteractionDisabled>());
        assert_eq!(world.resource::<InputFocus>().get(), None);
    }
}
