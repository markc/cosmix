//! Complete DCS application chrome composed from CTK's existing menu, chrome,
//! and dual-carousel sidebar components.

use bevy::app::{App, Plugin};
use bevy::ecs::entity::Entity;
use bevy::ecs::system::Commands;
use bevy::prelude::{default, Component, FlexDirection, Node};
use bevy::ui::{percent, px};

#[cfg(feature = "icons")]
use bevy::feathers::theme::UiTheme;

use crate::chrome::ChromePlugin;
use crate::dcs::{spawn_dcs_shell, DcsShellEntities, DcsShellPlugin, DcsShellProps};
#[cfg(feature = "icons")]
use crate::dcs::{
    DcsSidebarControlVisuals, DCS_TOP_CONTROL_PIN_ICON_PX, DCS_TOP_CONTROL_TOGGLE_ICON_PX,
};
#[cfg(feature = "icons")]
use crate::icons::{spawn_icon, Icon, IconSet};
#[cfg(feature = "menus")]
use crate::menu::MenuBarPlugin;
#[cfg(feature = "icons")]
use crate::theme::tokens;

/// Application-supplied slots for a complete DCS application shell.
///
/// Slot entities remain application-owned and are reparented into the shell.
/// Every supplied slot entity — menu, toolbar, centre, status, panel
/// contents, and sidebar control visuals — must be pairwise distinct;
/// reusing one silently empties the earlier slot when it is reparented.
/// Debug builds assert this; release builds do not check.
/// Mutable DCS state remains on the inner root returned as
/// [`DcsAppShellEntities::dcs`].
pub struct DcsAppShellProps {
    /// Optional menu bar above the DCS body.
    #[cfg(feature = "menus")]
    pub menu_bar: Option<Entity>,
    /// Optional application-level status bar below the DCS body.
    pub status_bar: Option<Entity>,
    /// Existing DCS configuration, including toolbar, panels, and centre.
    pub dcs: DcsShellProps,
}

impl DcsAppShellProps {
    /// Construct a shell without optional menu or status rows.
    pub fn new(dcs: DcsShellProps) -> Self {
        Self {
            #[cfg(feature = "menus")]
            menu_bar: None,
            status_bar: None,
            dcs,
        }
    }

    /// Add a menu bar above the DCS body.
    #[cfg(feature = "menus")]
    pub fn with_menu_bar(mut self, menu_bar: Entity) -> Self {
        self.menu_bar = Some(menu_bar);
        self
    }

    /// Add an application-level status bar below the DCS body.
    pub fn with_status_bar(mut self, status_bar: Entity) -> Self {
        self.status_bar = Some(status_bar);
        self
    }
}

/// Marker on the outer root of a complete DCS application shell.
#[derive(Component)]
pub struct DcsAppShell;

/// Stable entity handles returned by [`spawn_dcs_app_shell`].
pub struct DcsAppShellEntities {
    /// Whole-window outer column.
    pub root: Entity,
    /// Flex-growing host between the optional menu and status rows.
    pub body: Entity,
    /// Application-supplied menu slot.
    #[cfg(feature = "menus")]
    pub menu_bar: Option<Entity>,
    /// Application-supplied toolbar slot.
    pub toolbar: Entity,
    /// Application-supplied centre slot.
    pub centre: Entity,
    /// Application-supplied status slot.
    pub status_bar: Option<Entity>,
    /// Inner DCS entities. [`crate::dcs::DcsShellState`] lives on `dcs.root`.
    pub dcs: DcsShellEntities,
}

/// Spawn complete DCS application chrome around application-owned slot
/// entities.
///
/// The inner DCS root is parented unchanged beneath a flex-growing body host.
/// Applications must use the returned `dcs.root` when reading or mutating
/// [`crate::dcs::DcsShellState`].
pub fn spawn_dcs_app_shell(
    commands: &mut Commands,
    props: DcsAppShellProps,
) -> DcsAppShellEntities {
    #[cfg(feature = "menus")]
    let menu_bar = props.menu_bar;
    let status_bar = props.status_bar;
    let toolbar = props.dcs.toolbar;
    let centre = props.dcs.centre;
    #[cfg(debug_assertions)]
    {
        let mut slots: Vec<(String, Entity)> = Vec::new();
        #[cfg(feature = "menus")]
        if let Some(menu_bar) = menu_bar {
            slots.push(("menu bar".into(), menu_bar));
        }
        slots.push(("toolbar".into(), toolbar));
        slots.push(("centre".into(), centre));
        if let Some(status_bar) = status_bar {
            slots.push(("status bar".into(), status_bar));
        }
        for (side, panels) in [
            ("left", &props.dcs.left_panels),
            ("right", &props.dcs.right_panels),
        ] {
            for panel in panels {
                slots.push((format!("{side} panel '{}'", panel.id), panel.content));
            }
        }
        for (side, controls) in [
            ("left", &props.dcs.left_controls),
            ("right", &props.dcs.right_controls),
        ] {
            if let Some(controls) = controls {
                slots.push((format!("{side} controls toggle"), controls.toggle));
                slots.push((format!("{side} controls pinned"), controls.pinned));
                slots.push((format!("{side} controls floating"), controls.floating));
            }
        }
        debug_assert_distinct_slots(&slots);
    }
    let dcs = spawn_dcs_shell(commands, props.dcs);

    let body = commands
        .spawn(Node {
            width: percent(100),
            min_width: px(0),
            min_height: px(0),
            flex_grow: 1.0,
            flex_direction: FlexDirection::Column,
            ..default()
        })
        .add_child(dcs.root)
        .id();

    let mut children = Vec::with_capacity(3);
    #[cfg(feature = "menus")]
    if let Some(menu_bar) = menu_bar {
        children.push(menu_bar);
    }
    children.push(body);
    if let Some(status_bar) = status_bar {
        children.push(status_bar);
    }

    let root = commands
        .spawn((
            Node {
                width: percent(100),
                height: percent(100),
                min_width: px(0),
                min_height: px(0),
                flex_direction: FlexDirection::Column,
                ..default()
            },
            DcsAppShell,
        ))
        .add_children(&children)
        .id();

    DcsAppShellEntities {
        root,
        body,
        #[cfg(feature = "menus")]
        menu_bar,
        toolbar,
        centre,
        status_bar,
        dcs,
    }
}

/// Spawn complete DCS application chrome with CTK's standard icon visuals for
/// any sidebar whose controls were not supplied by the application.
#[cfg(feature = "icons")]
pub fn spawn_dcs_app_shell_with_icons(
    commands: &mut Commands,
    mut props: DcsAppShellProps,
    icons: &IconSet,
    theme: &UiTheme,
) -> DcsAppShellEntities {
    if props.dcs.left_controls.is_none() {
        props.dcs.left_controls = Some(spawn_sidebar_controls(commands, icons, theme));
    }
    if props.dcs.right_controls.is_none() {
        props.dcs.right_controls = Some(spawn_sidebar_controls(commands, icons, theme));
    }
    spawn_dcs_app_shell(commands, props)
}

#[cfg(feature = "icons")]
fn spawn_sidebar_controls(
    commands: &mut Commands,
    icons: &IconSet,
    theme: &UiTheme,
) -> DcsSidebarControlVisuals {
    DcsSidebarControlVisuals::new(
        spawn_icon(
            commands,
            icons,
            theme,
            Icon::Menu,
            DCS_TOP_CONTROL_TOGGLE_ICON_PX,
            tokens::TEXT,
        ),
        spawn_icon(
            commands,
            icons,
            theme,
            Icon::Pin,
            DCS_TOP_CONTROL_PIN_ICON_PX,
            tokens::TEXT,
        ),
        spawn_icon(
            commands,
            icons,
            theme,
            Icon::PinOff,
            DCS_TOP_CONTROL_PIN_ICON_PX,
            tokens::TEXT_DIM,
        ),
    )
}

#[cfg(debug_assertions)]
fn debug_assert_distinct_slots(slots: &[(String, Entity)]) {
    for (index, (left_name, left)) in slots.iter().enumerate() {
        for (right_name, right) in &slots[index + 1..] {
            debug_assert_ne!(
                left, right,
                "DCS app shell slot entities must be pairwise distinct: \
                 {left_name} and {right_name} both use {left:?}"
            );
        }
    }
}

/// Shared runtime behaviour required by a complete DCS application shell.
pub struct DcsAppShellPlugin;

impl Plugin for DcsAppShellPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins((ChromePlugin, DcsShellPlugin));
        #[cfg(feature = "menus")]
        app.add_plugins(MenuBarPlugin);
    }
}

#[cfg(test)]
mod tests {
    use bevy::ecs::hierarchy::{ChildOf, Children};
    use bevy::ecs::system::RunSystemOnce;
    use bevy::ecs::world::World;

    use super::*;
    use crate::dcs::{DcsShellProps, DcsShellState};

    struct Fixture {
        world: World,
        shell: DcsAppShellEntities,
        #[cfg(feature = "menus")]
        menu: Option<Entity>,
        toolbar: Entity,
        centre: Entity,
        status: Option<Entity>,
    }

    fn fixture(#[cfg(feature = "menus")] with_menu: bool, with_status: bool) -> Fixture {
        let mut world = World::new();
        #[cfg(feature = "menus")]
        let menu = with_menu.then(|| world.spawn_empty().id());
        let toolbar = world.spawn_empty().id();
        let centre = world.spawn_empty().id();
        let status = with_status.then(|| world.spawn_empty().id());

        let shell = world
            .run_system_once(move |mut commands: Commands| {
                let props = DcsAppShellProps::new(DcsShellProps::new(
                    toolbar,
                    centre,
                    Vec::new(),
                    Vec::new(),
                ));
                #[cfg(feature = "menus")]
                let props = if let Some(menu) = menu {
                    props.with_menu_bar(menu)
                } else {
                    props
                };
                let props = if let Some(status) = status {
                    props.with_status_bar(status)
                } else {
                    props
                };
                spawn_dcs_app_shell(&mut commands, props)
            })
            .expect("shell spawn system should run");

        Fixture {
            world,
            shell,
            #[cfg(feature = "menus")]
            menu,
            toolbar,
            centre,
            status,
        }
    }

    fn child_ids(world: &World, parent: Entity) -> Vec<Entity> {
        world
            .entity(parent)
            .get::<Children>()
            .map(|children| children.iter().copied().collect())
            .unwrap_or_default()
    }

    #[test]
    fn child_ordering_covers_every_menu_status_combination() {
        #[cfg(feature = "menus")]
        for with_menu in [false, true] {
            for with_status in [false, true] {
                let fixture = fixture(with_menu, with_status);
                let mut expected = Vec::new();
                if let Some(menu) = fixture.menu {
                    expected.push(menu);
                }
                expected.push(fixture.shell.body);
                if let Some(status) = fixture.status {
                    expected.push(status);
                }
                assert_eq!(child_ids(&fixture.world, fixture.shell.root), expected);
            }
        }

        #[cfg(not(feature = "menus"))]
        for with_status in [false, true] {
            let fixture = fixture(with_status);
            let mut expected = vec![fixture.shell.body];
            if let Some(status) = fixture.status {
                expected.push(status);
            }
            assert_eq!(child_ids(&fixture.world, fixture.shell.root), expected);
        }
    }

    #[test]
    fn root_body_and_inner_dcs_keep_their_layout_contracts() {
        let fixture = fixture(
            #[cfg(feature = "menus")]
            true,
            true,
        );
        let root = fixture
            .world
            .entity(fixture.shell.root)
            .get::<Node>()
            .expect("outer root should have a Node");
        assert_eq!(root.width, percent(100));
        assert_eq!(root.height, percent(100));
        assert_eq!(root.min_width, px(0));
        assert_eq!(root.min_height, px(0));
        assert_eq!(root.flex_direction, FlexDirection::Column);

        let body = fixture
            .world
            .entity(fixture.shell.body)
            .get::<Node>()
            .expect("body should have a Node");
        assert_eq!(body.width, percent(100));
        assert_eq!(body.min_width, px(0));
        assert_eq!(body.min_height, px(0));
        assert_eq!(body.flex_grow, 1.0);
        assert_eq!(body.flex_direction, FlexDirection::Column);

        let dcs = fixture
            .world
            .entity(fixture.shell.dcs.root)
            .get::<Node>()
            .expect("inner DCS root should retain its Node");
        assert_eq!(dcs.width, percent(100));
        assert_eq!(dcs.height, percent(100));
        assert_eq!(dcs.min_width, px(0));
        assert_eq!(dcs.min_height, px(0));
        assert_eq!(dcs.flex_direction, FlexDirection::Column);
        assert!(fixture
            .world
            .entity(fixture.shell.dcs.root)
            .contains::<DcsShellState>());
    }

    #[test]
    fn toolbar_remains_in_dcs_top_bar_below_optional_menu() {
        let fixture = fixture(
            #[cfg(feature = "menus")]
            true,
            false,
        );
        assert_eq!(
            fixture
                .world
                .entity(fixture.toolbar)
                .get::<ChildOf>()
                .expect("toolbar should have a parent")
                .parent(),
            fixture.shell.dcs.top_bar
        );
        assert_eq!(
            fixture
                .world
                .entity(fixture.shell.dcs.root)
                .get::<ChildOf>()
                .expect("DCS root should have a parent")
                .parent(),
            fixture.shell.body
        );
        #[cfg(feature = "menus")]
        assert_eq!(
            child_ids(&fixture.world, fixture.shell.root),
            [fixture.menu.unwrap(), fixture.shell.body]
        );
    }

    #[test]
    fn returned_handles_echo_application_slots() {
        let fixture = fixture(
            #[cfg(feature = "menus")]
            true,
            true,
        );
        #[cfg(feature = "menus")]
        assert_eq!(fixture.shell.menu_bar, fixture.menu);
        assert_eq!(fixture.shell.toolbar, fixture.toolbar);
        assert_eq!(fixture.shell.centre, fixture.centre);
        assert_eq!(fixture.shell.status_bar, fixture.status);
    }

    #[test]
    #[should_panic(expected = "DCS app shell slot entities must be pairwise distinct")]
    fn duplicate_slot_entities_are_rejected_in_debug_builds() {
        let mut world = World::new();
        let shared = world.spawn_empty().id();
        let _ = world.run_system_once(move |mut commands: Commands| {
            spawn_dcs_app_shell(
                &mut commands,
                DcsAppShellProps::new(DcsShellProps::new(shared, shared, Vec::new(), Vec::new())),
            )
        });
    }

    #[test]
    fn plugin_stack_initialises_headless() {
        let mut app = App::new();
        app.add_plugins(bevy::MinimalPlugins);
        #[cfg(feature = "menus")]
        app.init_resource::<bevy::feathers::theme::UiTheme>()
            .init_resource::<bevy::input_focus::InputFocus>();
        app.add_plugins(DcsAppShellPlugin);
        app.finish();
        app.cleanup();
        app.update();
    }
}
