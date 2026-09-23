//! Edge-generic CTK chrome from
//! `_plan/2026-08-06-cosmix-shell-corner-panels.md` §E1, §E2 and §E5, plus
//! the carousel chrome of `_doc/2026-09-22-quoin-panel-behavior-design.md`
//! §7–§8: chevrons and title per edge layout, inset from the panel ends by
//! the configured hotspot size, with slide-only page motion.
//!
//! The chrome owns no shell semantics and performs no window queries. A host
//! supplies four mount entities; every visual update is driven only by the
//! renderer-neutral [`ShellFrameState`]. Panels keep their final layout size.
//! The development host lets chrome own the complete off-edge slide, while a
//! layer-shell host selects protocol margins for overlay motion and leaves
//! only docked motion with [`UiTransform::translation`].

use accesskit::Role;
pub mod corner_menu;
use std::error::Error;
use std::fmt::{Display as FmtDisplay, Formatter};
use std::time::Duration;

use bevy::a11y::AccessibilityNode;
use bevy::app::{App, Plugin, Update};
use bevy::ecs::observer::On;
use bevy::ecs::system::SystemParam;
use bevy::input_focus::InputFocus;
use bevy::input_focus::tab_navigation::{TabGroup, TabIndex};
use bevy::picking::Pickable;
use bevy::picking::hover::Hovered;
use bevy::prelude::*;
use bevy::time::Real;
use bevy::ui::{InteractionDisabled, UiRect, Val, Val2, percent, px};
use bevy::ui_widgets::{Activate, Button as WidgetButton, ButtonPlugin as WidgetButtonPlugin};
use bevy::window::RequestRedraw;
use ctk::theme::{Mode, Scheme, ThemeSpec, ThemeState, tokens};

use crate::core::{Carousel, CarouselError, Edge, Orientation, PanelInput, PanelMode};
use crate::runtime::{
    CarouselInput, PageChange, ShellCommand, ShellCommandKind, ShellFrame, ShellFrameState,
    ShellRuntimeSet,
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
    /// Protocol margins translate overlays; chrome translates docked panels.
    ProtocolWhenUndocked,
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
/// Only Docked owns chrome translation. Hidden (including transient reveal)
/// and Pinned share protocol motion, so this latch deliberately needs no
/// transient flag; the complete committed presentation retains that intent.
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
    /// transient and pinned overlay motion; chrome owns docked motion.
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
            motion_ownership: QuoinMotionOwnership::ProtocolWhenUndocked,
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

/// Native host hit-tests this rendered strip before dispatching ordinary buttons.
/// Its computed transform includes the committed-motion chrome translation.
#[derive(Component)]
pub struct QuoinResizeGrip(pub Edge);

const RESIZE_GRIP_PX: f32 = 6.0;

/// Carousel slide duration — panel doc §8's `duration-slow` (the DCS
/// starting point, tuned in use).
pub const CAROUSEL_SLIDE: Duration = Duration::from_millis(300);
/// Carousel cleanup deadline (panel doc §8): slightly longer than the slide
/// so the outgoing page retires only after the slide has completed.
pub const CAROUSEL_CLEANUP: Duration = Duration::from_millis(320);

/// The effective carousel slide duration: [`CAROUSEL_SLIDE`], collapsed to
/// zero under reduced motion. Panel doc §8: a zero duration is handled
/// directly — DCS's `0.01ms` exists only because browsers never fire
/// `transitionend` at zero, and is not imported here.
pub const fn carousel_slide_duration(reduced_motion: bool) -> Duration {
    if reduced_motion {
        Duration::ZERO
    } else {
        CAROUSEL_SLIDE
    }
}

/// The compositor's corner-hotspot size in logical pixels — comp's
/// `input.corners.deadzone_px`, mirrored over the Bus (decided 2026-09-23,
/// refactor open question 4: comp owns hotspots, so its configured deadzone
/// is the one authority). The Quoin host's Bus service observes the property
/// and replaces this resource; the fallback default mirrors comp's own
/// default deadzone and applies only until the first observation arrives,
/// or for as long as no cosmix comp is present.
#[derive(Resource, Clone, Copy, Debug)]
pub struct QuoinHotspotSize(pub f32);

/// Mirrors comp's default in `cosmix-comp/src/protocol/corner.rs` (12 logical
/// pixels), until the first Bus observation or while no cosmix comp is present.
pub const DEFAULT_COMP_HOTSPOT_PX: f32 = 12.0;

impl Default for QuoinHotspotSize {
    fn default() -> Self {
        Self(DEFAULT_COMP_HOTSPOT_PX)
    }
}

/// Accessibility seam for carousel motion (panel doc §8): reduced motion
/// collapses the slide to zero. Hosts insert this when the user asks for
/// reduced motion; the default is full motion.
#[derive(Resource, Clone, Copy, Debug, Default)]
pub struct QuoinReducedMotion(pub bool);

/// One edge's in-flight carousel slide (panel doc §8).
#[derive(Clone, Debug, Eq, PartialEq)]
struct QuoinSlide {
    /// The page sliding out, kept displayed until cleanup retires it.
    outgoing: String,
    /// Travel direction along the panel's long axis.
    forward: bool,
    started_at: Duration,
}

/// Chrome-side carousel slide state, indexed by [`Edge::index`].
#[derive(Resource, Default)]
struct QuoinCarouselSlides([Option<QuoinSlide>; 4]);

#[derive(Component)]
struct QuoinPanelChrome {
    edge: Edge,
    motion_ownership: QuoinMotionOwnership,
    pointer_ownership: QuoinPointerOwnership,
}

#[derive(Component)]
struct QuoinPanelParts {
    /// Side panels: the `< [title] >` header bar at the top. Horizontal
    /// panels: the centred title-and-dots overlay across the content strip.
    header: Entity,
    /// `[previous, next]` chevron buttons. Side panels keep them in the
    /// header; horizontal panels pin them at the two panel ends. Hidden with
    /// the header while the active page is chromeless.
    chevrons: [Entity; 2],
    title_label: Entity,
    dots_host: Entity,
    page_host: Entity,
    page_titles: Vec<(String, String)>,
    page_chromeless: Vec<(String, bool)>,
    page_wrappers: Vec<(String, Entity)>,
    dot_labels: Vec<(String, Entity)>,
    controls: Vec<Entity>,
    /// The active page id this panel last presented; diffs against the frame
    /// to detect page switches and pick their motion kind.
    presented_page: Option<String>,
}

#[derive(SystemParam)]
struct PresentPanelQueries<'w, 's> {
    panels: Query<
        'w,
        's,
        (
            &'static QuoinPanelChrome,
            &'static mut QuoinPanelParts,
            &'static mut Node,
            &'static mut UiTransform,
        ),
    >,
    nodes: Query<'w, 's, &'static mut Node, Without<QuoinPanelChrome>>,
    transforms: Query<'w, 's, &'static mut UiTransform, Without<QuoinPanelChrome>>,
    labels: Query<'w, 's, &'static mut Text>,
    tab_indices: Query<'w, 's, &'static mut TabIndex>,
    disabled_controls: Query<'w, 's, Has<InteractionDisabled>>,
}

#[derive(Component, Clone)]
struct QuoinControl {
    edge: Edge,
    action: QuoinAction,
}

#[derive(Component)]
struct QuoinControlPage(String);

#[derive(Clone)]
enum QuoinAction {
    Scheme(Scheme),
    Intent,
    Quit,
    Previous,
    Next,
    Select(String),
}

/// Installs semantic chrome input and ShellFrame-only reconciliation.
pub struct QuoinChromePlugin;

impl Plugin for QuoinChromePlugin {
    fn build(&self, app: &mut App) {
        // The widget observer set (Pointer<Click> -> Activate on ui_widgets
        // Button entities): production hosts already get it — DefaultPlugins
        // includes UiWidgetsPlugins under the bevy_ui_widgets feature this
        // crate enables — so there the guard skips. Self-registering covers
        // plugin-less compositions (bare-App tests, slimmed hosts). Feathers'
        // same-named ButtonPlugin is styles-only; don't mistake it for this.
        // ORDERING: Bevy panics on duplicate plugins, and this guard only
        // sees EARLIER registrations — add DefaultPlugins/UiWidgetsPlugins/
        // WidgetButtonPlugin BEFORE QuoinChromePlugin, never after it.
        if !app.is_plugin_added::<WidgetButtonPlugin>() {
            app.add_plugins(WidgetButtonPlugin);
        }
        app.init_resource::<InputFocus>()
            .init_resource::<QuoinCarouselSlides>()
            .init_resource::<QuoinHotspotSize>()
            .init_resource::<QuoinReducedMotion>()
            .add_message::<QuoinSchemeSelected>()
            // Chrome requests redraws even in hosts without WindowPlugin.
            .add_message::<RequestRedraw>()
            .add_observer(on_activate)
            .add_systems(
                Update,
                (panel_hover, escape_panels)
                    .chain()
                    .in_set(ShellRuntimeSet::Input),
            )
            .add_systems(
                Update,
                (
                    present_panels,
                    present_page_controls,
                    present_content,
                    present_navlinks,
                    present_resize_grips,
                )
                    .chain()
                    .in_set(ShellRuntimeSet::Presentation),
            )
            .add_systems(
                PostUpdate,
                present_scheme_dots.before(bevy::ui::UiSystems::Layout),
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

/// Mount a dynamic page without rebuilding other pages or their editing state.
/// Returns false until the host has created this edge's chrome.
pub fn mount_page(world: &mut World, edge: Edge, id: &str, title: &str, content: Entity) -> bool {
    mount_page_with(world, edge, id, title, content, false)
}

/// Register or update content without revealing or selecting it. Optionally
/// hide the header when the page is explicitly selected later.
pub fn mount_page_with(
    world: &mut World,
    edge: Edge,
    id: &str,
    title: &str,
    content: Entity,
    chromeless: bool,
) -> bool {
    let mut query = world.query::<(Entity, &QuoinPanelChrome)>();
    let Some(panel) = query
        .iter(world)
        .find(|(_, chrome)| chrome.edge == edge)
        .map(|(entity, _)| entity)
    else {
        return false;
    };
    let (host, dots) = {
        let parts = world.get::<QuoinPanelParts>(panel).unwrap();
        (parts.page_host, parts.dots_host)
    };
    let exists = world
        .get::<QuoinPanelParts>(panel)
        .unwrap()
        .page_wrappers
        .iter()
        .any(|(page, _)| page == id);
    if exists {
        let mut parts = world.get_mut::<QuoinPanelParts>(panel).unwrap();
        parts.page_chromeless.retain(|(page, _)| page != id);
        parts.page_chromeless.push((id.into(), chromeless));
        if let Some((_, current)) = world
            .get_mut::<QuoinPanelParts>(panel)
            .unwrap()
            .page_titles
            .iter_mut()
            .find(|(page, _)| page == id)
        {
            *current = title.into();
        }
        return true;
    }
    let wrapper = world
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                left: px(0),
                top: px(0),
                width: percent(100),
                height: percent(100),
                display: Display::None,
                ..default()
            },
            UiTransform::default(),
        ))
        .add_child(content)
        .id();
    world.entity_mut(host).add_child(wrapper);
    let mut queue = bevy::ecs::world::CommandQueue::default();
    let mut commands = Commands::new(&mut queue, world);
    let label = text(&mut commands, "○", 11.0, true);
    let dot = button(
        &mut commands,
        edge,
        QuoinAction::Select(id.into()),
        label,
        &format!("Show {title}"),
    );
    commands.entity(dots).add_child(dot);
    queue.apply(world);
    let mut parts = world.get_mut::<QuoinPanelParts>(panel).unwrap();
    parts.controls.push(dot);
    parts.dot_labels.push((id.into(), label));
    parts.page_titles.push((id.into(), title.into()));
    parts.page_chromeless.push((id.into(), chromeless));
    parts.page_wrappers.push((id.into(), wrapper));
    crate::runtime::register_shell_page(world, edge, id);
    true
}

/// Remove a dynamic page and repair the carousel selection.
pub fn unmount_page(world: &mut World, edge: Edge, id: &str) {
    unmount_page_content(world, edge, id);
    crate::runtime::remove_shell_page(world, edge, id);
}

/// Tear down chrome after a registry removal has already applied the landing.
pub fn unmount_page_content(world: &mut World, edge: Edge, id: &str) {
    let mut query = world.query::<(Entity, &QuoinPanelChrome)>();
    let Some(panel) = query
        .iter(world)
        .find(|(_, chrome)| chrome.edge == edge)
        .map(|(e, _)| e)
    else {
        return;
    };
    let mut parts = world.get_mut::<QuoinPanelParts>(panel).unwrap();
    let dot_label = parts
        .dot_labels
        .iter()
        .find(|(page, _)| page == id)
        .map(|(_, e)| *e);
    parts.dot_labels.retain(|(page, _)| page != id);
    let wrapper = parts
        .page_wrappers
        .iter()
        .find(|(page, _)| page == id)
        .map(|(_, entity)| *entity);
    parts.page_titles.retain(|(page, _)| page != id);
    parts.page_chromeless.retain(|(page, _)| page != id);
    parts.page_wrappers.retain(|(page, _)| page != id);
    if let Some(label) = dot_label
        && let Some(parent) = world.get::<ChildOf>(label).map(ChildOf::parent)
    {
        world
            .get_mut::<QuoinPanelParts>(panel)
            .unwrap()
            .controls
            .retain(|e| *e != parent);
        world.despawn(parent);
    }
    if let Some(wrapper) = wrapper {
        world.despawn(wrapper);
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
    let mut controls = vec![previous, next];
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

    // Panel doc §7: side panels carry a `< [title] >` header at the top with
    // the same prev/next; horizontal panels leave the strip to content, with
    // the chevrons at the two panel ends and the title and dots as a centred
    // overlay.
    let horizontal = edge.orientation() == Orientation::Horizontal;
    let header = if horizontal {
        commands
            .spawn((
                Node {
                    position_type: PositionType::Absolute,
                    left: px(0),
                    top: px(0),
                    width: percent(100),
                    height: percent(100),
                    flex_direction: FlexDirection::Row,
                    align_items: AlignItems::Center,
                    justify_content: JustifyContent::Center,
                    column_gap: px(4),
                    ..default()
                },
                // The strip belongs to content: only the title and the dots
                // pick, so the overlay itself must not.
                Pickable::IGNORE,
                ZIndex(1),
            ))
            .add_children(&[title_label, dots])
            .id()
    } else {
        commands
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
            .add_children(&[previous, title_label, dots, next])
            .id()
    };

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
            .spawn((
                // Stacked pages (panel doc §8): the slide translates two
                // wrappers inside the same clipped rectangle, so every
                // wrapper is absolutely positioned at the host's full size
                // and rests at translation zero.
                Node {
                    position_type: PositionType::Absolute,
                    left: px(0),
                    top: px(0),
                    width: percent(100),
                    height: percent(100),
                    display: if index == 0 {
                        Display::Flex
                    } else {
                        Display::None
                    },
                    ..default()
                },
                UiTransform::default(),
            ))
            .add_child(page.content)
            .id();
        commands.entity(page_host).add_child(wrapper);
        page_wrappers.push((page.id, wrapper));
    }

    let root_children = if horizontal {
        vec![previous, page_host, next, header]
    } else {
        vec![header, page_host]
    };
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
                padding: hotspot_padding(edge, QuoinHotspotSize::default()),
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
                header,
                chevrons: [previous, next],
                title_label,
                dots_host: dots,
                page_host,
                page_titles,
                page_chromeless: Vec::new(),
                page_wrappers,
                dot_labels,
                controls,
                presented_page: None,
            },
        ))
        .add_children(&root_children)
        .id();
    let grip = commands
        .spawn((
            QuoinResizeGrip(edge),
            Node {
                position_type: PositionType::Absolute,
                left: if edge == Edge::Left { Val::Auto } else { px(0) },
                right: if edge == Edge::Right {
                    Val::Auto
                } else {
                    px(0)
                },
                top: if edge == Edge::Top { Val::Auto } else { px(0) },
                bottom: if edge == Edge::Bottom {
                    Val::Auto
                } else {
                    px(0)
                },
                width: if horizontal {
                    Val::Auto
                } else {
                    px(RESIZE_GRIP_PX)
                },
                height: if horizontal {
                    px(RESIZE_GRIP_PX)
                } else {
                    Val::Auto
                },
                ..default()
            },
            Pickable::default(),
            Hovered::default(),
            BackgroundColor(Color::NONE),
            ZIndex(10),
        ))
        .id();
    commands.entity(root).add_child(grip);
    commands.entity(mount).add_child(root);
}

fn present_resize_grips(
    frame: Res<ShellFrameState>,
    mut grips: Query<(&QuoinResizeGrip, &Hovered, &mut BackgroundColor)>,
) {
    for (grip, hovered, mut colour) in &mut grips {
        let alpha = if frame.0.panel(grip.0).resize_active {
            0.8
        } else if hovered.0 {
            0.5
        } else {
            0.0
        };
        colour.0 = Color::srgba(0.3, 0.65, 1.0, alpha);
    }
}

fn panel_border(edge: Edge) -> UiRect {
    match edge {
        Edge::Left => UiRect::right(px(1)),
        Edge::Bottom => UiRect::top(px(1)),
        Edge::Right => UiRect::left(px(1)),
        Edge::Top => UiRect::bottom(px(1)),
    }
}

/// Inset of the carousel furniture from the panel ends (panel doc §7): the
/// extreme ends of a horizontal panel and the top end of a side panel sit
/// under a corner hotspot, where a control is unclickable. Spawn uses the
/// fallback size; presentation re-reads the observed one every frame.
fn hotspot_padding(edge: Edge, size: QuoinHotspotSize) -> UiRect {
    match edge.orientation() {
        Orientation::Horizontal => UiRect::horizontal(px(size.0)),
        Orientation::Vertical => UiRect::top(px(size.0)),
    }
}

/// Slide translation for one page wrapper as a percentage of its own size
/// along the panel's long axis (panel doc §8, constant-speed like the panel
/// motion): the incoming page travels from beyond its edge to rest, the
/// outgoing from rest out the opposite edge, in lockstep.
fn slide_translation(edge: Edge, forward: bool, progress: f32, outgoing: bool) -> Val2 {
    let sign = if forward { 1.0 } else { -1.0 };
    let fraction = if outgoing { -progress } else { 1.0 - progress };
    let offset = Val::Percent(sign * fraction * 100.0);
    match edge.orientation() {
        Orientation::Horizontal => Val2::new(offset, Val::ZERO),
        Orientation::Vertical => Val2::new(Val::ZERO, offset),
    }
}

/// A quit control follows the same semantic command path as Bus quit.
pub fn quoin_quit_button(commands: &mut Commands, edge: Edge, page: &str) -> Entity {
    let label = text(commands, "Quit Quoin", 14.0, false);
    let entity = button(commands, edge, QuoinAction::Quit, label, "Quit Quoin");
    commands
        .entity(entity)
        .insert(QuoinControlPage(page.to_owned()));
    entity
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
            // ui_widgets::Button, NOT the bevy::prelude Button (legacy
            // bevy_ui marker): the Activate-producing click observers filter
            // on the ui_widgets type. The prelude one compiled and rendered
            // fine while making every control click-dead (found live
            // 2026-09-06; the test fixtures inserted the right type by hand,
            // which is why unit tests never caught it). Same trap already
            // documented at ctk/src/file_requester.rs — "left every requester
            // button silently inert".
            WidgetButton,
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

/// A page target must be a valid carousel identity; places may defer their action.
#[derive(Clone)]
pub enum NavLinkTarget {
    Page(String),
    Intent,
}

/// One active row per page-local intent group; page links follow the model.
#[derive(Component)]
pub struct NavLink {
    icon: Entity,
    label: Entity,
    active: bool,
}

/// Labels collapse below rail thickness without changing the accessible name.
pub fn navlink(
    commands: &mut Commands,
    edge: Edge,
    page: &str,
    icon: &str,
    label: &str,
    target: NavLinkTarget,
) -> Result<Entity, CarouselError> {
    Carousel::new([page])?;
    let action = match target {
        NavLinkTarget::Page(id) => {
            Carousel::new([id.as_str()])?;
            QuoinAction::Select(id)
        }
        NavLinkTarget::Intent => QuoinAction::Intent,
    };
    let glyph = text(commands, icon, 15.0, true);
    let caption = text(commands, label, 14.0, true);
    let entity = button(commands, edge, action, glyph, label);
    commands.entity(entity).add_child(caption).insert((
        Node {
            width: percent(100),
            min_height: px(28),
            flex_shrink: 0.0,
            padding: UiRect::axes(px(8), px(4)),
            column_gap: px(8),
            align_items: AlignItems::Center,
            border_radius: BorderRadius::all(px(6)),
            ..default()
        },
        QuoinControlPage(page.to_owned()),
        NavLink {
            icon: glyph,
            label: caption,
            active: false,
        },
    ));
    Ok(entity)
}

fn navlink_tokens(
    active: bool,
    hovered: bool,
) -> (
    bevy::feathers::theme::ThemeToken,
    bevy::feathers::theme::ThemeToken,
) {
    (
        if active || hovered {
            tokens::ROW_HOVER
        } else {
            tokens::PANEL
        },
        if active {
            tokens::CONTROL_ACTIVE
        } else {
            tokens::TEXT_DIM
        },
    )
}

fn present_navlinks(
    mut commands: Commands,
    frame: Res<ShellFrameState>,
    links: Query<(
        Entity,
        &QuoinControl,
        &NavLink,
        &Hovered,
        &bevy::feathers::theme::ThemeBackgroundColor,
    )>,
    mut labels: Query<(&mut Node, &bevy::feathers::theme::ThemeTextColor), Without<NavLink>>,
) {
    for (entity, control, link, hovered, background) in &links {
        let panel = frame.0.panel(control.edge);
        let active = match &control.action {
            QuoinAction::Select(id) => panel.active_page_id.as_deref() == Some(id),
            _ => link.active,
        };
        let (bg, fg) = navlink_tokens(active, hovered.0 && panel.mapped);
        if background.0 != bg {
            commands
                .entity(entity)
                .insert(bevy::feathers::theme::ThemeBackgroundColor(bg));
        }
        for entity in [link.icon, link.label] {
            if let Ok((mut node, color)) = labels.get_mut(entity) {
                if color.0 != fg {
                    commands
                        .entity(entity)
                        .insert(bevy::feathers::theme::ThemeTextColor(fg.clone()));
                }
                node.display = if entity == link.label && panel.thickness_px < 110.0 {
                    Display::None
                } else {
                    Display::Flex
                };
            }
        }
    }
}

/// Only an enabled, visible scheme control emits a selection.
#[derive(Message, Clone, Copy, Debug)]
pub struct QuoinSchemeSelected(pub Scheme);

#[derive(Component)]
struct SchemeDot(Scheme);

/// Preview colours are independent of the currently applied palette.
pub fn scheme_dot(
    commands: &mut Commands,
    edge: Edge,
    page: &str,
    scheme: Scheme,
) -> Result<Entity, CarouselError> {
    Carousel::new([page])?;
    let label = text(commands, "", 1.0, true);
    let entity = button(
        commands,
        edge,
        QuoinAction::Scheme(scheme),
        label,
        scheme.name(),
    );
    commands
        .entity(entity)
        .remove::<bevy::feathers::theme::ThemeBackgroundColor>()
        .insert((
            Node {
                width: px(26),
                height: px(26),
                flex_shrink: 0.0,
                border: UiRect::all(px(2)),
                border_radius: BorderRadius::MAX,
                ..default()
            },
            BackgroundColor(
                ThemeSpec::from_scheme(scheme, Mode::Dark)
                    .colors
                    .control_active,
            ),
            BorderColor::all(Color::NONE),
            SchemeDot(scheme),
            QuoinControlPage(page.to_owned()),
        ));
    Ok(entity)
}

fn present_scheme_dots(
    theme: Option<Res<ThemeState>>,
    mut dots: Query<(&SchemeDot, &mut BorderColor)>,
) {
    let Some(theme) = theme else {
        return;
    };
    for (dot, mut border) in &mut dots {
        *border = BorderColor::all(if dot.0 == theme.scheme {
            Color::WHITE
        } else {
            Color::NONE
        });
    }
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

#[derive(SystemParam)]
struct ChromeRequests<'w> {
    commands: MessageWriter<'w, ShellCommand>,
    redraw: MessageWriter<'w, RequestRedraw>,
    schemes: MessageWriter<'w, QuoinSchemeSelected>,
}

fn on_activate(
    activated: On<Activate>,
    controls: Query<(
        &QuoinControl,
        Has<InteractionDisabled>,
        Option<&QuoinControlPage>,
    )>,
    frame: Res<ShellFrameState>,
    time: Res<Time<Real>>,
    mut requests: ChromeRequests,
    mut links: Query<(Entity, &QuoinControl, &QuoinControlPage, &mut NavLink)>,
) {
    let Ok((control, disabled, page)) = controls.get(activated.entity) else {
        return;
    };
    if disabled
        || !frame.0.panel(control.edge).mapped
        || page.is_some_and(|page| {
            frame.0.panel(control.edge).active_page_id.as_deref() != Some(page.0.as_str())
        })
    {
        return;
    }
    let kind = match &control.action {
        QuoinAction::Scheme(scheme) => {
            requests.schemes.write(QuoinSchemeSelected(*scheme));
            requests.redraw.write(RequestRedraw);
            return;
        }
        QuoinAction::Intent => {
            for (entity, other, other_page, mut link) in &mut links {
                if other.edge == control.edge
                    && page.is_some_and(|page| page.0 == other_page.0)
                    && matches!(other.action, QuoinAction::Intent)
                {
                    link.active = entity == activated.entity;
                }
            }
            requests.redraw.write(RequestRedraw);
            return;
        }
        QuoinAction::Quit => ShellCommandKind::Quit,
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
    requests.commands.write(ShellCommand {
        output: frame.0.geometry.output.clone(),
        at: time.elapsed(),
        kind,
    });
    // Activate is produced by the widget layer during Update and may occur
    // after the model set. Guarantee one follow-up pass in reactive mode.
    requests.redraw.write(RequestRedraw);
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

/// Optional presentation settings `present_panels` reads when present.
type PresentSettings<'w> = (
    Option<Res<'w, QuoinHotspotSize>>,
    Option<Res<'w, QuoinReducedMotion>>,
    Option<Res<'w, QuoinCommittedMotionModes>>,
);

fn present_panels(
    mut commands: Commands,
    frame: Res<ShellFrameState>,
    (hotspot, reduced_motion, committed_modes): PresentSettings,
    (time, mut slides, mut redraw): (
        Res<Time<Real>>,
        ResMut<QuoinCarouselSlides>,
        MessageWriter<RequestRedraw>,
    ),
    mut focus: ResMut<InputFocus>,
    mut queries: PresentPanelQueries,
) {
    let now = time.elapsed();
    let duration =
        carousel_slide_duration(reduced_motion.is_some_and(|reduced_motion| reduced_motion.0));
    for (chrome, mut parts, mut node, mut transform) in &mut queries.panels {
        let panel = frame.0.panel(chrome.edge);
        let chromeless = parts
            .page_chromeless
            .iter()
            .any(|(id, chromeless)| *chromeless && panel.active_page_id.as_deref() == Some(id));
        let controls_enabled = panel.mapped && !chromeless;
        let display = if panel.mapped {
            Display::Flex
        } else {
            Display::None
        };
        if node.display != display {
            node.display = display;
        }
        // Chevron inset (panel doc §7), re-read every frame so an observed
        // comp deadzone moves the furniture live.
        let padding = hotspot_padding(
            chrome.edge,
            hotspot.as_deref().copied().unwrap_or_default(),
        );
        if node.padding != padding {
            node.padding = padding;
        }
        for control in &parts.controls {
            if let Ok(mut tab_index) = queries.tab_indices.get_mut(*control) {
                let index = if controls_enabled { 0 } else { -1 };
                if tab_index.0 != index {
                    tab_index.0 = index;
                }
            }
            let disabled = queries.disabled_controls.get(*control).unwrap_or(false);
            if controls_enabled && disabled {
                commands.entity(*control).remove::<InteractionDisabled>();
            } else if !controls_enabled && !disabled {
                commands.entity(*control).insert(InteractionDisabled);
            }
            if !controls_enabled && focus.get() == Some(*control) {
                focus.clear();
            }
        }
        let chrome_owns_motion = match chrome.motion_ownership {
            QuoinMotionOwnership::Chrome => true,
            QuoinMotionOwnership::ProtocolWhenUndocked => {
                committed_modes
                    .as_ref()
                    .expect("layer mounts require QuoinCommittedMotionModes")
                    .get(chrome.edge)
                    == PanelMode::Docked
            }
        };
        let hidden = if chrome_owns_motion {
            (1.0 - panel.visible_fraction) * panel.thickness_px
        } else {
            0.0
        };
        let translation = match chrome.edge {
            Edge::Left => Val2::new(px(-hidden), px(0)),
            Edge::Bottom => Val2::new(px(0), px(hidden)),
            Edge::Right => Val2::new(px(hidden), px(0)),
            Edge::Top => Val2::new(px(0), px(-hidden)),
        };
        if transform.translation != translation {
            transform.translation = translation;
        }
        if let Some(title) = panel.active_page_id.as_deref().and_then(|active| {
            parts
                .page_titles
                .iter()
                .find_map(|(id, title)| (id == active).then_some(title))
        }) && let Ok(mut label) = queries.labels.get_mut(parts.title_label)
            && label.0 != *title
        {
            label.0.clone_from(title);
        }
        if let Ok(mut header) = queries.nodes.get_mut(parts.header) {
            let display = if chromeless {
                Display::None
            } else {
                Display::Flex
            };
            if header.display != display {
                header.display = display;
            }
        }
        // Chromeless pages hide the whole carousel furniture: on horizontal
        // panels the chevrons sit outside the header and hide with it.
        for chevron in parts.chevrons {
            if let Ok(mut chevron_node) = queries.nodes.get_mut(chevron) {
                let display = if chromeless {
                    Display::None
                } else {
                    Display::Flex
                };
                if chevron_node.display != display {
                    chevron_node.display = display;
                }
            }
        }
        // Carousel motion (panel doc §5, §8): only a sequential change —
        // chevron paging, including its wrap-around — slides. A named jump
        // (dots, `page.set`, activate, restore, removal landing) and reduced
        // motion switch directly.
        let index = chrome.edge.index();
        if panel.active_page_id != parts.presented_page {
            let outgoing = parts.presented_page.take();
            let sequential = matches!(panel.page_change, PageChange::Sequential { .. });
            slides.0[index] = match outgoing {
                Some(outgoing)
                    if sequential
                        && !duration.is_zero()
                        && panel.active_page_id.is_some()
                        && parts
                            .page_wrappers
                            .iter()
                            .any(|(id, _)| *id == outgoing) =>
                {
                    Some(QuoinSlide {
                        outgoing,
                        forward: matches!(
                            panel.page_change,
                            PageChange::Sequential { forward: true }
                        ),
                        started_at: now,
                    })
                }
                _ => None,
            };
            parts.presented_page = panel.active_page_id.clone();
        }
        let mut slide = slides.0[index].take();
        if duration.is_zero() || panel.page_change == PageChange::Named || slide
            .as_ref()
            .is_some_and(|slide| now.saturating_sub(slide.started_at) >= CAROUSEL_CLEANUP)
        {
            // Cleanup (panel doc §8): the outgoing page retires only after
            // the slide has completed.
            slide = None;
        }
        let progress = slide.as_ref().map(|slide| {
            (now.saturating_sub(slide.started_at).as_secs_f32() / duration.as_secs_f32()).min(1.0)
        });
        for (id, entity) in &parts.page_wrappers {
            if let Ok(mut page_node) = queries.nodes.get_mut(*entity) {
                let mut display = if panel.active_page_id.as_deref() == Some(id) {
                    Display::Flex
                } else {
                    Display::None
                };
                if slide
                    .as_ref()
                    .is_some_and(|slide| slide.outgoing == *id)
                {
                    display = Display::Flex;
                }
                if page_node.display != display {
                    page_node.display = display;
                }
            }
            let translation = match &slide {
                Some(slide) if slide.outgoing == *id => {
                    slide_translation(chrome.edge, slide.forward, progress.unwrap_or(1.0), true)
                }
                Some(slide) if panel.active_page_id.as_deref() == Some(id) => {
                    slide_translation(chrome.edge, slide.forward, progress.unwrap_or(1.0), false)
                }
                _ => Val2::default(),
            };
            if let Ok(mut wrapper_transform) = queries.transforms.get_mut(*entity)
                && wrapper_transform.translation != translation
            {
                wrapper_transform.translation = translation;
            }
        }
        if let Some(slide) = slide {
            slides.0[index] = Some(slide);
            // Keep frames flowing in reactive hosts until cleanup retires
            // the outgoing page.
            redraw.write(RequestRedraw);
        }
        for (id, entity) in &parts.dot_labels {
            if let Ok(mut label) = queries.labels.get_mut(*entity) {
                let text = if panel.active_page_id.as_deref() == Some(id) {
                    "●"
                } else {
                    "○"
                };
                if label.0 != text {
                    label.0 = text.to_owned();
                }
            }
        }
    }
}

fn present_page_controls(
    mut commands: Commands,
    frame: Res<ShellFrameState>,
    mut focus: ResMut<InputFocus>,
    mut controls: Query<(
        Entity,
        &QuoinControl,
        &QuoinControlPage,
        &mut TabIndex,
        Has<InteractionDisabled>,
    )>,
) {
    for (entity, control, page, mut tab, disabled) in &mut controls {
        let panel = frame.0.panel(control.edge);
        let enabled = panel.mapped && panel.active_page_id.as_deref() == Some(page.0.as_str());
        tab.0 = if enabled { 0 } else { -1 };
        if enabled && disabled {
            commands.entity(entity).remove::<InteractionDisabled>();
        } else if !enabled && !disabled {
            commands.entity(entity).insert(InteractionDisabled);
        }
        if !enabled && focus.get() == Some(entity) {
            focus.clear();
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
    fn navlink_active_and_hover_resolve_independently() {
        assert_eq!(
            navlink_tokens(false, false),
            (tokens::PANEL, tokens::TEXT_DIM)
        );
        assert_eq!(
            navlink_tokens(false, true),
            (tokens::ROW_HOVER, tokens::TEXT_DIM)
        );
        for hovered in [false, true] {
            assert_eq!(
                navlink_tokens(true, hovered),
                (tokens::ROW_HOVER, tokens::CONTROL_ACTIVE)
            );
        }
    }

    #[test]
    fn navlink_label_tracks_panel_thickness_and_active_theme_binding() {
        let registry = QuoinPageRegistry::new(
            vec![spec("nav")],
            vec![spec("launcher")],
            vec![spec("monitor")],
            vec![spec("status")],
        )
        .unwrap();
        let mut app = App::new();
        app.insert_resource(ShellFrameState(frame_for(&registry)));
        let mut queue = bevy::ecs::world::CommandQueue::default();
        let entity = navlink(
            &mut Commands::new(&mut queue, app.world()),
            Edge::Left,
            "nav",
            "⊞",
            "Apps",
            NavLinkTarget::Page("nav".into()),
        )
        .unwrap();
        queue.apply(app.world_mut());
        let label = app.world().get::<NavLink>(entity).unwrap().label;
        for (thickness, display) in [(109.0, Display::None), (110.0, Display::Flex)] {
            app.world_mut().resource_mut::<ShellFrameState>().0.panels[Edge::Left.index()]
                .thickness_px = thickness;
            app.world_mut().run_system_once(present_navlinks).unwrap();
            assert_eq!(app.world().get::<Node>(label).unwrap().display, display);
            assert_eq!(
                app.world()
                    .get::<bevy::feathers::theme::ThemeTextColor>(label)
                    .unwrap()
                    .0,
                tokens::CONTROL_ACTIVE
            );
        }
    }

    #[test]
    fn quit_button_only_activates_on_its_visible_page() {
        let registry = QuoinPageRegistry::new(
            vec![spec("nav")],
            vec![spec("launcher"), spec("power")],
            vec![spec("monitor"), spec("agents")],
            vec![spec("status")],
        )
        .unwrap();
        let mut frame = frame_for(&registry);
        frame.panels[Edge::Right.index()].mapped = true;
        let mut app = App::new();
        app.insert_resource(ShellFrameState(frame))
            .insert_resource(Time::<Real>::default())
            .init_resource::<InputFocus>()
            .add_message::<ShellCommand>()
            .add_message::<RequestRedraw>()
            .add_message::<QuoinSchemeSelected>()
            .add_observer(on_activate);
        let mut queue = bevy::ecs::world::CommandQueue::default();
        let entity = quoin_quit_button(
            &mut Commands::new(&mut queue, app.world()),
            Edge::Right,
            "monitor",
        );
        queue.apply(app.world_mut());
        app.world_mut()
            .run_system_once(present_page_controls)
            .unwrap();
        assert!(!app.world().entity(entity).contains::<InteractionDisabled>());
        app.world_mut().trigger(Activate { entity });
        assert_eq!(
            app.world_mut()
                .resource_mut::<Messages<ShellCommand>>()
                .drain()
                .next()
                .unwrap()
                .kind,
            ShellCommandKind::Quit
        );
        app.world_mut().resource_mut::<ShellFrameState>().0.panels[Edge::Right.index()]
            .active_page_id = Some("agents".into());
        app.world_mut()
            .run_system_once(present_page_controls)
            .unwrap();
        assert!(app.world().entity(entity).contains::<InteractionDisabled>());
        app.world_mut().trigger(Activate { entity });
        assert!(app.world().resource::<Messages<ShellCommand>>().is_empty());
    }

    #[test]
    fn pointer_click_quit_uses_semantic_command() {
        assert_eq!(
            pointer_click_command(QuoinAction::Quit),
            ShellCommandKind::Quit
        );
    }

    #[test]
    fn pointer_cursor_press_release_activates_both_chevrons_and_dot() {
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
    fn repeated_panel_frame_preserves_change_ticks_but_real_updates_propagate() {
        let registry = QuoinPageRegistry::new(
            vec![spec("nav"), spec("places")],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )
        .unwrap();
        let mut frame = frame_for(&registry);
        frame.panels[Edge::Left.index()].mapped = true;
        frame.panels[Edge::Left.index()].mode = PanelMode::Hidden;
        frame.panels[Edge::Left.index()].transient_revealed = true;
        let mut world = World::new();
        let title = world.spawn(Text::new("")).id();
        let nav_dot = world.spawn(Text::new("")).id();
        let places_dot = world.spawn(Text::new("")).id();
        let nav_page = world.spawn((Node::default(), UiTransform::default())).id();
        let places_page = world.spawn((Node::default(), UiTransform::default())).id();
        let control = world.spawn(TabIndex(-1)).id();
        let panel = world
            .spawn((
                QuoinPanelChrome {
                    edge: Edge::Left,
                    motion_ownership: QuoinMotionOwnership::ProtocolWhenUndocked,
                    pointer_ownership: QuoinPointerOwnership::NativeSurface,
                },
                QuoinPanelParts {
                    header: Entity::PLACEHOLDER,
                    chevrons: [Entity::PLACEHOLDER; 2],
                    title_label: title,
                    dots_host: Entity::PLACEHOLDER,
                    page_host: Entity::PLACEHOLDER,
                    page_chromeless: Vec::new(),
                    page_titles: vec![
                        ("nav".into(), "Navigation".into()),
                        ("places".into(), "Places".into()),
                    ],
                    page_wrappers: vec![("nav".into(), nav_page), ("places".into(), places_page)],
                    dot_labels: vec![("nav".into(), nav_dot), ("places".into(), places_dot)],
                    controls: vec![control],
                    presented_page: None,
                },
                Node::default(),
                UiTransform::default(),
            ))
            .id();
        world.insert_resource(ShellFrameState(frame));
        world.insert_resource(InputFocus::default());
        world.insert_resource(QuoinCommittedMotionModes::hidden());
        world.insert_resource(Time::<Real>::default());
        world.insert_resource(QuoinCarouselSlides::default());
        world.init_resource::<bevy::ecs::message::Messages<RequestRedraw>>();
        world.run_system_once(present_panels).unwrap();
        world.clear_trackers();
        world.run_system_once(present_panels).unwrap();
        for entity in [title, nav_dot, places_dot] {
            assert!(!world.entity(entity).get_ref::<Text>().unwrap().is_changed());
        }
        for entity in [panel, nav_page, places_page] {
            assert!(!world.entity(entity).get_ref::<Node>().unwrap().is_changed());
        }
        assert!(
            !world
                .entity(panel)
                .get_ref::<UiTransform>()
                .unwrap()
                .is_changed()
        );
        assert!(
            !world
                .entity(control)
                .get_ref::<TabIndex>()
                .unwrap()
                .is_changed()
        );

        // Carousel changes must still reach the existing entities.
        {
            let mut frame = world.resource_mut::<ShellFrameState>();
            let left = &mut frame.0.panels[Edge::Left.index()];
            left.mode = PanelMode::Docked;
            left.transient_revealed = false;
            left.active_page_id = Some("places".into());
        }
        world.run_system_once(present_panels).unwrap();
        for entity in [title, nav_dot, places_dot] {
            assert!(world.entity(entity).get_ref::<Text>().unwrap().is_changed());
        }
        assert_eq!(world.get::<Text>(title).unwrap().0, "Places");
        assert_eq!(world.get::<Node>(nav_page).unwrap().display, Display::None);
        assert_eq!(
            world.get::<Node>(places_page).unwrap().display,
            Display::Flex
        );
        assert!(
            world
                .entity(nav_page)
                .get_ref::<Node>()
                .unwrap()
                .is_changed()
        );
        assert!(
            world
                .entity(places_page)
                .get_ref::<Node>()
                .unwrap()
                .is_changed()
        );

        world.clear_trackers();
        {
            let mut frame = world.resource_mut::<ShellFrameState>();
            let left = &mut frame.0.panels[Edge::Left.index()];
            left.mode = PanelMode::Hidden;
            left.mapped = false;
        }
        world.run_system_once(present_panels).unwrap();
        assert_eq!(world.get::<Node>(panel).unwrap().display, Display::None);
        assert!(world.entity(panel).get_ref::<Node>().unwrap().is_changed());
        assert_eq!(world.get::<TabIndex>(control).unwrap().0, -1);
        assert!(
            world
                .entity(control)
                .get_ref::<TabIndex>()
                .unwrap()
                .is_changed()
        );
        assert!(world.get::<InteractionDisabled>(control).is_some());
    }

    #[test]
    fn layer_mounts_leave_overlay_motion_to_protocol_but_own_docked_motion() {
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
        let title_label = world.spawn(Text::new("Panel")).id();
        let chrome = world
            .spawn((
                QuoinPanelChrome {
                    edge: Edge::Left,
                    motion_ownership: QuoinMotionOwnership::ProtocolWhenUndocked,
                    pointer_ownership: QuoinPointerOwnership::NativeSurface,
                },
                QuoinPanelParts {
                    header: Entity::PLACEHOLDER,
                    chevrons: [Entity::PLACEHOLDER; 2],
                    title_label,
                    dots_host: Entity::PLACEHOLDER,
                    page_host: Entity::PLACEHOLDER,
                    page_chromeless: Vec::new(),
                    page_titles: Vec::new(),
                    page_wrappers: Vec::new(),
                    dot_labels: Vec::new(),
                    controls: Vec::new(),
                    presented_page: None,
                },
                Node::default(),
                UiTransform::default(),
            ))
            .id();
        world.insert_resource(ShellFrameState(ShellFrame::from_model(&model)));
        world.insert_resource(QuoinCommittedMotionModes::hidden());
        world.insert_resource(InputFocus::default());
        world.insert_resource(Time::<Real>::default());
        world.insert_resource(QuoinCarouselSlides::default());
        world.init_resource::<bevy::ecs::message::Messages<RequestRedraw>>();
        world.run_system_once(present_panels).unwrap();
        assert_eq!(
            world.get::<UiTransform>(chrome).unwrap().translation,
            Val2::new(px(0), px(0))
        );

        // Persistent pin stays on the same Overlay layer and protocol motion
        // path as transient reveal, before AND after its latch update.
        model
            .panel_input(Edge::Left, Duration::ZERO, PanelInput::Pin)
            .unwrap();
        world.resource_mut::<ShellFrameState>().0 = ShellFrame::from_model(&model);
        world.run_system_once(present_panels).unwrap();
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
            Val2::new(px(0), px(0))
        );

        model
            .panel_input(Edge::Left, Duration::ZERO, PanelInput::Dock)
            .unwrap();
        world.resource_mut::<ShellFrameState>().0 = ShellFrame::from_model(&model);
        world.run_system_once(present_panels).unwrap();
        // Current docked with committed overlay remains protocol-owned.
        assert_eq!(
            world.get::<UiTransform>(chrome).unwrap().translation,
            Val2::new(px(0), px(0))
        );
        world
            .resource_mut::<QuoinCommittedMotionModes>()
            .set(Edge::Left, PanelMode::Docked);
        world.run_system_once(present_panels).unwrap();
        assert_eq!(
            world.get::<UiTransform>(chrome).unwrap().translation,
            Val2::new(px(-240.0), px(0))
        );
        model
            .panel_input(Edge::Left, Duration::ZERO, PanelInput::Undock)
            .unwrap();
        world.resource_mut::<ShellFrameState>().0 = ShellFrame::from_model(&model);
        world.run_system_once(present_panels).unwrap();
        // Current overlay with committed docked remains chrome-owned.
        assert_eq!(
            world.get::<UiTransform>(chrome).unwrap().translation,
            Val2::new(px(-240.0), px(0))
        );
        world
            .resource_mut::<QuoinCommittedMotionModes>()
            .set(Edge::Left, PanelMode::Hidden);
        world.run_system_once(present_panels).unwrap();
        assert_eq!(
            world.get::<UiTransform>(chrome).unwrap().translation,
            Val2::new(px(0), px(0))
        );
        assert_eq!(model.panel(Edge::Left).visible_fraction, 0.0);
        assert_eq!(model.panel(Edge::Left).exclusive_zone_px, 0.0);
    }

    #[test]
    fn hidden_header_controls_are_not_reachable_by_tab_focus() {
        use bevy::input_focus::tab_navigation::{NavAction, TabNavigation};
        let model = ShellModel::new(
            OutputKey::new("test").unwrap(),
            LogicalSize::new(1_000.0, 800.0).unwrap(),
            Duration::ZERO,
            Duration::from_millis(300),
            Duration::from_millis(180),
        )
        .unwrap();
        let mut frame = ShellFrame::from_model(&model);
        frame.panels[Edge::Left.index()].mapped = true;
        frame.panels[Edge::Left.index()].active_page_id = Some("plain".into());
        let mut world = World::new();
        let control = world.spawn(TabIndex(0)).id();
        let content_control = world.spawn(TabIndex(0)).id();
        let header = world.spawn(Node::default()).add_child(control).id();
        let title_label = world.spawn(Text::new("Panel")).id();
        world
            .spawn((
                QuoinPanelChrome {
                    edge: Edge::Left,
                    motion_ownership: QuoinMotionOwnership::Chrome,
                    pointer_ownership: QuoinPointerOwnership::ChromeHover,
                },
                QuoinPanelParts {
                    header,
                    chevrons: [Entity::PLACEHOLDER; 2],
                    title_label,
                    dots_host: Entity::PLACEHOLDER,
                    page_host: Entity::PLACEHOLDER,
                    page_chromeless: vec![("plain".into(), true)],
                    page_titles: Vec::new(),
                    page_wrappers: Vec::new(),
                    dot_labels: Vec::new(),
                    controls: vec![control],
                    presented_page: None,
                },
                Node::default(),
                UiTransform::default(),
                TabGroup::new(0),
            ))
            .add_children(&[header, content_control]);
        world.insert_resource(ShellFrameState(frame));
        world.insert_resource(InputFocus::from_entity(control));
        world.insert_resource(Time::<Real>::default());
        world.insert_resource(QuoinCarouselSlides::default());
        world.init_resource::<bevy::ecs::message::Messages<RequestRedraw>>();
        world.run_system_once(present_panels).unwrap();
        assert_eq!(world.get::<Node>(header).unwrap().display, Display::None);
        assert_eq!(world.get::<TabIndex>(control), Some(&TabIndex(-1)));
        assert!(world.entity(control).contains::<InteractionDisabled>());
        assert_eq!(world.resource::<InputFocus>().get(), None);
        let next = world
            .run_system_once(|nav: TabNavigation, focus: Res<InputFocus>| {
                nav.navigate(&focus, NavAction::Next).unwrap()
            })
            .unwrap();
        assert_eq!(next, content_control);
        world.resource_mut::<ShellFrameState>().0.panels[Edge::Left.index()].active_page_id =
            Some("normal".into());
        world.run_system_once(present_panels).unwrap();
        assert_eq!(world.get::<Node>(header).unwrap().display, Display::Flex);
        assert_eq!(world.get::<TabIndex>(control), Some(&TabIndex(0)));
        assert!(!world.entity(control).contains::<InteractionDisabled>());
        let next = world
            .run_system_once(|nav: TabNavigation, focus: Res<InputFocus>| {
                nav.navigate(&focus, NavAction::Next).unwrap()
            })
            .unwrap();
        assert_eq!(next, control);
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
        let title_label = world.spawn(Text::new("Panel")).id();
        world.spawn((
            QuoinPanelChrome {
                edge: Edge::Left,
                motion_ownership: QuoinMotionOwnership::Chrome,
                pointer_ownership: QuoinPointerOwnership::ChromeHover,
            },
            QuoinPanelParts {
                header: Entity::PLACEHOLDER,
                chevrons: [Entity::PLACEHOLDER; 2],
                title_label,
                dots_host: Entity::PLACEHOLDER,
                page_host: Entity::PLACEHOLDER,
                page_chromeless: Vec::new(),
                page_titles: Vec::new(),
                page_wrappers: Vec::new(),
                dot_labels: Vec::new(),
                controls: vec![control],
                presented_page: None,
            },
            Node::default(),
            UiTransform::default(),
        ));
        world.insert_resource(ShellFrameState(mapped));
        world.insert_resource(InputFocus::default());
        world.insert_resource(Time::<Real>::default());
        world.insert_resource(QuoinCarouselSlides::default());
        world.init_resource::<bevy::ecs::message::Messages<RequestRedraw>>();
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

    #[test]
    fn chrome_registers_redraw_messages_without_window_plugin() {
        let mut app = App::new();
        app.add_plugins(QuoinChromePlugin);
        assert!(app.world().contains_resource::<Messages<RequestRedraw>>());
    }

    /// Type-identity regression for the 2026-09-06 click-dead bug: the
    /// production spawn path must attach `bevy::ui_widgets::Button` (the type
    /// the Activate observers filter on) — the prelude `Button` renders and
    /// hovers identically while never activating. The plugin half asserts the
    /// plugin-less-host fallback: production gets the observers from
    /// DefaultPlugins' UiWidgetsPlugins, bare compositions from
    /// QuoinChromePlugin's own guarded registration.
    #[test]
    fn controls_carry_the_widget_button_type_and_observer_plugin() {
        let mut app = App::new();
        app.add_plugins(QuoinChromePlugin);
        assert!(
            app.is_plugin_added::<WidgetButtonPlugin>(),
            "QuoinChromePlugin must register the ui_widgets ButtonPlugin"
        );
        let label = app.world_mut().spawn_empty().id();
        let mut commands = app.world_mut().commands();
        let control = button(&mut commands, Edge::Right, QuoinAction::Quit, label, "quit");
        app.world_mut().flush();
        assert!(
            app.world().entity(control).contains::<WidgetButton>(),
            "chrome controls must carry bevy::ui_widgets::Button, not the prelude Button"
        );
        assert!(
            !app.world()
                .entity(control)
                .contains::<bevy::ui::widget::Button>(),
            "the legacy prelude Button must not creep back into the bundle"
        );
    }

    /// Full-spawn sweep: every control the production `spawn_quoin_chrome`
    /// path produces — chevrons, dots, page controls on all four
    /// edges — must carry the ui_widgets Button. Guards the assumption that
    /// `button()` stays the single spawn choke point; a control spawned some
    /// other way would pass the choke-point test above and still be
    /// click-dead in production.
    #[test]
    fn every_spawned_control_carries_the_widget_button_type() {
        let mut app = App::new();
        app.add_plugins(QuoinChromePlugin);
        let world = app.world_mut();
        let mounts = QuoinPanelMounts::new(
            world.spawn_empty().id(),
            world.spawn_empty().id(),
            world.spawn_empty().id(),
            world.spawn_empty().id(),
        );
        let panels = std::array::from_fn(|index| {
            vec![QuoinPage {
                id: format!("page-{index}"),
                title: format!("Page {index}"),
                content: world.spawn_empty().id(),
            }]
        });
        let props = QuoinChromeProps { panels };
        let mut commands = app.world_mut().commands();
        spawn_quoin_chrome(&mut commands, mounts, props);
        app.world_mut().flush();
        let mut parts_query = app.world_mut().query::<&QuoinPanelParts>();
        let controls: Vec<Entity> = parts_query
            .iter(app.world())
            .flat_map(|parts| parts.controls.iter().copied())
            .collect();
        assert!(
            !controls.is_empty(),
            "spawn_quoin_chrome produced no controls — the sweep would be vacuous"
        );
        assert_eq!(
            controls.len(),
            12,
            "only two chevrons and one page dot per header; no mode control"
        );
        for control in controls {
            assert!(
                app.world().entity(control).contains::<WidgetButton>(),
                "control {control} lacks bevy::ui_widgets::Button — it renders but can never Activate"
            );
        }
    }

    #[derive(Resource)]
    struct TestPanelEntity(Entity);

    /// A production-spawned panel on one edge, with the frame's carousel on
    /// that edge only and time primed so `advance` moves the clock.
    fn panel_world(edge: Edge, page_ids: &[&str]) -> World {
        let mut panels: [Vec<QuoinPageSpec>; 4] = std::array::from_fn(|_| Vec::new());
        panels[edge.index()] = page_ids.iter().map(|id| spec(id)).collect();
        let registry = QuoinPageRegistry::new(
            panels[0].clone(),
            panels[1].clone(),
            panels[2].clone(),
            panels[3].clone(),
        )
        .unwrap();
        let mut world = World::new();
        world.insert_resource(ShellFrameState(frame_for(&registry)));
        world.insert_resource(InputFocus::default());
        let mut time = Time::<Real>::default();
        time.update_with_duration(Duration::ZERO);
        world.insert_resource(time);
        world.insert_resource(QuoinCarouselSlides::default());
        world.init_resource::<bevy::ecs::message::Messages<RequestRedraw>>();
        let mount = world.spawn_empty().id();
        let pages = page_ids
            .iter()
            .map(|id| QuoinPage {
                id: (*id).to_owned(),
                title: (*id).to_owned(),
                content: world.spawn_empty().id(),
            })
            .collect();
        let mut queue = bevy::ecs::world::CommandQueue::default();
        spawn_panel(
            &mut Commands::new(&mut queue, &world),
            mount,
            edge,
            QuoinMotionOwnership::Chrome,
            QuoinPointerOwnership::ChromeHover,
            pages,
        );
        queue.apply(&mut world);
        let mut query = world.query::<(Entity, &QuoinPanelChrome)>();
        let entity = query.iter(&world)
            .find(|(_, chrome)| chrome.edge == edge)
            .map(|(entity, _)| entity)
            .unwrap();
        world.insert_resource(TestPanelEntity(entity));
        world
    }

    fn panel_entity(world: &World, edge: Edge) -> Entity {
        let entity = world.resource::<TestPanelEntity>().0;
        assert_eq!(world.get::<QuoinPanelChrome>(entity).unwrap().edge, edge);
        entity
    }

    fn wrapper_of(world: &World, edge: Edge, id: &str) -> Entity {
        world
            .get::<QuoinPanelParts>(panel_entity(world, edge))
            .unwrap()
            .page_wrappers
            .iter()
            .find(|(page, _)| page == id)
            .map(|(_, entity)| *entity)
            .unwrap()
    }

    fn advance(world: &mut World, by: Duration) {
        world
            .resource_mut::<Time<Real>>()
            .update_with_duration(by);
    }

    fn switch_page(world: &mut World, edge: Edge, id: &str, change: PageChange) {
        let panel = &mut world.resource_mut::<ShellFrameState>().0.panels[edge.index()];
        panel.active_page_id = Some(id.to_owned());
        panel.page_change = change;
    }

    /// The chevron actions page sequentially and wrap in both directions,
    /// and the wrap still carries the sequential marker so it animates
    /// (panel doc §5: chevrons are the sequential prev/next with DCS's
    /// directional wrap-around; a named jump never animates).
    #[test]
    fn chevrons_wrap_at_both_ends() {
        use crate::runtime::ShellRuntimePlugin;

        let mut model = ShellModel::new(
            OutputKey::new("test").unwrap(),
            LogicalSize::new(1_000.0, 800.0).unwrap(),
            Duration::ZERO,
            Duration::from_millis(300),
            Duration::from_millis(180),
        )
        .unwrap();
        model.set_carousel(Edge::Left, Carousel::new(["first", "last"]).unwrap());
        let mut app = App::new();
        app.add_plugins((
            bevy::MinimalPlugins,
            ShellRuntimePlugin::new(model),
            QuoinChromePlugin,
        ))
        .init_resource::<ButtonInput<KeyCode>>()
        .add_message::<RequestRedraw>();
        let output = app
            .world()
            .resource::<ShellFrameState>()
            .0
            .geometry
            .output
            .clone();
        let page = |app: &mut App, input: CarouselInput| {
            app.world_mut().write_message(ShellCommand {
                output: output.clone(),
                at: Duration::ZERO,
                kind: ShellCommandKind::Carousel {
                    edge: Edge::Left,
                    input,
                },
            });
            app.update();
            app.world()
                .resource::<ShellFrameState>()
                .0
                .panel(Edge::Left)
                .clone()
        };
        assert_eq!(
            page(&mut app, CarouselInput::Next).active_page_id.as_deref(),
            Some("last")
        );
        // Next off the last page wraps to the first and still animates.
        let wrapped = page(&mut app, CarouselInput::Next);
        assert_eq!(wrapped.active_page_id.as_deref(), Some("first"));
        assert_eq!(
            wrapped.page_change,
            PageChange::Sequential { forward: true }
        );
        // Previous off the first page wraps back to the last.
        let wrapped = page(&mut app, CarouselInput::Previous);
        assert_eq!(wrapped.active_page_id.as_deref(), Some("last"));
        assert_eq!(
            wrapped.page_change,
            PageChange::Sequential { forward: false }
        );
        assert_eq!(
            page(&mut app, CarouselInput::Previous)
                .active_page_id
                .as_deref(),
            Some("first")
        );
        // A named selection of the already-active page still cancels a slide.
        assert_eq!(
            page(&mut app, CarouselInput::SelectId("first".into())).page_change,
            PageChange::Named
        );
        for edge in Edge::ALL {
            for forward in [false, true] {
                let sign = if forward { 1.0 } else { -1.0 };
                let translation = |offset| match edge.orientation() {
                    Orientation::Horizontal => Val2::new(Val::Percent(offset), Val::ZERO),
                    Orientation::Vertical => Val2::new(Val::ZERO, Val::Percent(offset)),
                };
                assert_eq!(slide_translation(edge, forward, 0.5, true), translation(-50.0 * sign));
                assert_eq!(slide_translation(edge, forward, 0.5, false), translation(50.0 * sign));
            }
        }
    }

    /// The chevrons are inset from the panel ends by the configured hotspot
    /// size, never flush (panel doc §7): the ends of a horizontal panel and
    /// the top of a side panel sit under a corner hotspot. The inset tracks
    /// comp's observed `input.corners.deadzone_px` live, falling back to the
    /// comp default only while unobserved.
    #[test]
    fn chevron_inset_reads_hotspot_size() {
        let expected = |edge: Edge, size: f32| hotspot_padding(edge, QuoinHotspotSize(size));
        for edge in Edge::ALL {
            let mut world = panel_world(edge, &["alpha"]);
            let panel = panel_entity(&world, edge);
            world.run_system_once(present_panels).unwrap();
            assert_eq!(
                world.get::<Node>(panel).unwrap().padding,
                expected(edge, DEFAULT_COMP_HOTSPOT_PX),
                "{edge:?}: unobserved fallback must mirror comp's default deadzone"
            );
            for observed in [24.0, 40.0] {
                world.insert_resource(QuoinHotspotSize(observed));
                world.run_system_once(present_panels).unwrap();
                assert_eq!(
                    world.get::<Node>(panel).unwrap().padding,
                    expected(edge, observed),
                    "{edge:?}: the inset must follow the observed deadzone, not a constant"
                );
            }
        }
    }

    /// Panel doc §8: the slide is 300 ms and collapses to zero under reduced
    /// motion — a zero duration handled directly, not DCS's browser
    /// workaround. Reduced motion switches pages with no slide state and no
    /// leftover offset.
    #[test]
    fn slide_duration_is_300ms_and_zero_when_reduced_motion() {
        assert_eq!(
            carousel_slide_duration(false),
            Duration::from_millis(300)
        );
        assert_eq!(carousel_slide_duration(true), Duration::ZERO);
        assert!(
            CAROUSEL_CLEANUP > CAROUSEL_SLIDE,
            "cleanup must run after the slide completes"
        );

        let mut world = panel_world(Edge::Left, &["alpha", "beta"]);
        world.insert_resource(QuoinReducedMotion(true));
        world.run_system_once(present_panels).unwrap();
        advance(&mut world, Duration::from_millis(50));
        switch_page(
            &mut world,
            Edge::Left,
            "beta",
            PageChange::Sequential { forward: true },
        );
        world.run_system_once(present_panels).unwrap();
        assert_eq!(
            world.get::<Node>(wrapper_of(&world, Edge::Left, "alpha"))
                .unwrap()
                .display,
            Display::None,
            "reduced motion retires the outgoing page immediately"
        );
        let beta = wrapper_of(&world, Edge::Left, "beta");
        assert_eq!(world.get::<Node>(beta).unwrap().display, Display::Flex);
        assert_eq!(
            world.get::<UiTransform>(beta).unwrap().translation,
            Val2::default()
        );
        assert!(world
            .resource::<QuoinCarouselSlides>()
            .0[Edge::Left.index()]
            .is_none());

        // Switching the preference while a slide is running also lands now.
        world.insert_resource(QuoinReducedMotion(false));
        switch_page(&mut world, Edge::Left, "alpha", PageChange::Sequential { forward: false });
        world.run_system_once(present_panels).unwrap();
        assert!(world.resource::<QuoinCarouselSlides>().0[Edge::Left.index()].is_some());
        world.insert_resource(QuoinReducedMotion(true));
        world.run_system_once(present_panels).unwrap();
        assert!(world.resource::<QuoinCarouselSlides>().0[Edge::Left.index()].is_none());
        assert_eq!(world.get::<Node>(beta).unwrap().display, Display::None);
    }

    /// A wrap-around slide keeps the outgoing page rendered until the 320 ms
    /// cleanup deadline — past the 300 ms slide — retires it (panel doc §8).
    #[test]
    fn wrap_cleanup_runs_after_slide_completes() {
        let mut world = panel_world(Edge::Left, &["alpha", "beta"]);
        world.run_system_once(present_panels).unwrap();
        // Rest on the last page, then page forward: a wrap to the first.
        switch_page(&mut world, Edge::Left, "beta", PageChange::None);
        world.run_system_once(present_panels).unwrap();
        advance(&mut world, Duration::from_millis(100));
        switch_page(
            &mut world,
            Edge::Left,
            "alpha",
            PageChange::Sequential { forward: true },
        );
        world.run_system_once(present_panels).unwrap();
        let outgoing = wrapper_of(&world, Edge::Left, "beta");
        let incoming = wrapper_of(&world, Edge::Left, "alpha");
        // Slide start: both render; the incoming page enters from beyond
        // its edge, the outgoing one rests.
        assert_eq!(world.get::<Node>(outgoing).unwrap().display, Display::Flex);
        assert_eq!(
            world.get::<UiTransform>(incoming).unwrap().translation,
            Val2::new(Val::ZERO, Val::Percent(100.0))
        );

        advance(&mut world, CAROUSEL_SLIDE);
        world.run_system_once(present_panels).unwrap();
        assert_eq!(
            world.get::<UiTransform>(incoming).unwrap().translation,
            Val2::default(),
            "the wrap slide must reach rest at its duration"
        );
        assert_eq!(
            world.get::<UiTransform>(outgoing).unwrap().translation,
            Val2::new(Val::ZERO, Val::Percent(-100.0)),
            "the outgoing page must be fully off-panel"
        );
        assert_eq!(
            world.get::<Node>(outgoing).unwrap().display,
            Display::Flex,
            "the slide has completed but the cleanup deadline has not"
        );

        advance(&mut world, CAROUSEL_CLEANUP - CAROUSEL_SLIDE - Duration::from_millis(1));
        world.run_system_once(present_panels).unwrap();
        assert_eq!(
            world.get::<Node>(outgoing).unwrap().display,
            Display::Flex,
            "one millisecond short of the cleanup deadline"
        );

        advance(&mut world, Duration::from_millis(1));
        world.run_system_once(present_panels).unwrap();
        assert_eq!(
            world.get::<Node>(outgoing).unwrap().display,
            Display::None,
            "cleanup retires the outgoing page after the slide completes"
        );
        assert_eq!(
            world.get::<UiTransform>(outgoing).unwrap().translation,
            Val2::default()
        );
        assert!(world
            .resource::<QuoinCarouselSlides>()
            .0[Edge::Left.index()]
            .is_none());
    }

    /// A named jump — dots, `page.set`, activate (panel doc §5) — switches
    /// with no slide, before and during a running one.
    #[test]
    fn named_jump_is_not_animated() {
        for change in [PageChange::Named, PageChange::None] {
            let mut world = panel_world(Edge::Bottom, &["alpha", "beta"]);
            world.run_system_once(present_panels).unwrap();
            advance(&mut world, Duration::from_millis(10));
            switch_page(&mut world, Edge::Bottom, "beta", change);
            world.run_system_once(present_panels).unwrap();
            assert_eq!(
                world.get::<Node>(wrapper_of(&world, Edge::Bottom, "alpha"))
                    .unwrap()
                    .display,
                Display::None,
                "{change:?}: a named jump retires the outgoing page immediately"
            );
            let beta = wrapper_of(&world, Edge::Bottom, "beta");
            assert_eq!(world.get::<Node>(beta).unwrap().display, Display::Flex);
            assert_eq!(
                world.get::<UiTransform>(beta).unwrap().translation,
                Val2::default()
            );
            assert!(world
                .resource::<QuoinCarouselSlides>()
                .0[Edge::Bottom.index()]
                .is_none());
        }

        // A named jump mid-flight discards the running slide and lands.
        let mut world = panel_world(Edge::Bottom, &["alpha", "beta", "gamma"]);
        world.run_system_once(present_panels).unwrap();
        switch_page(
            &mut world,
            Edge::Bottom,
            "beta",
            PageChange::Sequential { forward: true },
        );
        world.run_system_once(present_panels).unwrap();
        advance(&mut world, Duration::from_millis(100));
        switch_page(&mut world, Edge::Bottom, "gamma", PageChange::Named);
        world.run_system_once(present_panels).unwrap();
        assert_eq!(
            world.get::<Node>(wrapper_of(&world, Edge::Bottom, "beta"))
                .unwrap()
                .display,
            Display::None
        );
        let gamma = wrapper_of(&world, Edge::Bottom, "gamma");
        assert_eq!(world.get::<Node>(gamma).unwrap().display, Display::Flex);
        assert_eq!(
            world.get::<UiTransform>(gamma).unwrap().translation,
            Val2::default()
        );
        assert!(world
            .resource::<QuoinCarouselSlides>()
            .0[Edge::Bottom.index()]
            .is_none());

        // Naming the current incoming page must stop its existing slide too.
        switch_page(&mut world, Edge::Bottom, "alpha", PageChange::Sequential { forward: true });
        world.run_system_once(present_panels).unwrap();
        assert!(world.resource::<QuoinCarouselSlides>().0[Edge::Bottom.index()].is_some());
        switch_page(&mut world, Edge::Bottom, "alpha", PageChange::Named);
        world.run_system_once(present_panels).unwrap();
        assert!(world.resource::<QuoinCarouselSlides>().0[Edge::Bottom.index()].is_none());
        let alpha = wrapper_of(&world, Edge::Bottom, "alpha");
        assert_eq!(world.get::<UiTransform>(alpha).unwrap().translation, Val2::default());
        assert_eq!(world.get::<Node>(gamma).unwrap().display, Display::None);
    }
}
