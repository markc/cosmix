//! Shared application chrome: toolbar rows and single-line status text.

use std::borrow::Cow;

use accesskit::Role;
use bevy::a11y::AccessibilityNode;
use bevy::app::{App, Plugin, Update};
use bevy::ecs::query::Changed;
#[cfg(feature = "actions")]
use bevy::ecs::query::Has;
#[cfg(feature = "actions")]
use bevy::ecs::system::Res;
use bevy::ecs::system::{Commands, Query};
#[cfg(feature = "icons")]
use bevy::feathers::theme::UiTheme;
use bevy::feathers::theme::{ThemeBackgroundColor, ThemeTextColor};
use bevy::input_focus::tab_navigation::TabIndex;
use bevy::prelude::{
    default, AlignItems, Button, Component, Entity, FlexDirection, JustifyContent, Name, Node,
    Text, TextFont, TextLayout,
};
#[cfg(feature = "actions")]
use bevy::ui::InteractionDisabled;
use bevy::ui::{percent, px, BorderRadius, Overflow, OverflowClipMargin, UiRect};

#[cfg(feature = "actions")]
use cosmix_actions::ActionId;

use crate::dcs::DCS_TOOLBAR_SAFE_PADDING_PX;
#[cfg(feature = "icons")]
use crate::icons::{spawn_icon, Icon, IconSet};
#[cfg(feature = "actions")]
use crate::menu::ActionRegistryResource;
use crate::theme::tokens;

/// Registry action carried by a toolbar button.
///
/// CTK keeps this separate from action dispatch: applications that already
/// distinguish keyboard, pointer, Bus and other ingress can preserve that
/// routing while sharing the chrome construction and enabled presentation.
#[cfg(feature = "actions")]
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub struct ToolbarActionButton(pub ActionId);

/// One toolbar button declaration.
pub struct ToolbarButtonDef {
    label: Cow<'static, str>,
    show_label: bool,
    #[cfg(feature = "icons")]
    icon: Option<Icon>,
    #[cfg(feature = "actions")]
    action: Option<ActionId>,
}

impl ToolbarButtonDef {
    /// Create a label-only button.
    pub const fn new(label: &'static str) -> Self {
        Self {
            label: Cow::Borrowed(label),
            show_label: true,
            #[cfg(feature = "icons")]
            icon: None,
            #[cfg(feature = "actions")]
            action: None,
        }
    }

    /// Create a button with a runtime-owned accessible label.
    pub fn with_dynamic_label(label: impl Into<String>) -> Self {
        Self {
            label: Cow::Owned(label.into()),
            show_label: true,
            #[cfg(feature = "icons")]
            icon: None,
            #[cfg(feature = "actions")]
            action: None,
        }
    }

    /// Attach a catalogue icon.
    #[cfg(feature = "icons")]
    pub const fn with_icon(mut self, icon: Icon) -> Self {
        self.icon = Some(icon);
        self
    }

    /// Hide the visible label while retaining it for accessibility and names.
    pub const fn icon_only(mut self) -> Self {
        self.show_label = false;
        self
    }

    /// Bind the button to an application action.
    #[cfg(feature = "actions")]
    pub const fn with_action(mut self, action: ActionId) -> Self {
        self.action = Some(action);
        self
    }
}

/// One member of a toolbar's left or right group.
pub enum ToolbarItem {
    /// A CTK-styled toolbar button.
    Button(ToolbarButtonDef),
    /// An application-supplied entity, such as shared status text.
    Entity(Entity),
}

impl From<ToolbarButtonDef> for ToolbarItem {
    fn from(button: ToolbarButtonDef) -> Self {
        Self::Button(button)
    }
}

impl From<Entity> for ToolbarItem {
    fn from(entity: Entity) -> Self {
        Self::Entity(entity)
    }
}

/// Entities created for a toolbar row.
pub struct ToolbarRowEntities {
    pub root: Entity,
    pub left: Vec<Entity>,
    pub right: Vec<Entity>,
}

/// Placement of the two groups within a toolbar row.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ToolbarAlignment {
    /// Keep the groups against opposite sides of the row.
    #[default]
    Edges,
    /// Centre both groups as one cluster while retaining their group gap.
    Centre,
}

/// Spawn a toolbar row without icon resources.
pub fn spawn_toolbar_row(
    commands: &mut Commands,
    left: impl IntoIterator<Item = ToolbarItem>,
    right: impl IntoIterator<Item = ToolbarItem>,
) -> ToolbarRowEntities {
    spawn_toolbar_row_aligned(commands, ToolbarAlignment::Edges, left, right)
}

/// Spawn a toolbar row with explicit group placement and without icon resources.
pub fn spawn_toolbar_row_aligned(
    commands: &mut Commands,
    alignment: ToolbarAlignment,
    left: impl IntoIterator<Item = ToolbarItem>,
    right: impl IntoIterator<Item = ToolbarItem>,
) -> ToolbarRowEntities {
    spawn_toolbar_row_inner(
        commands,
        alignment,
        left,
        right,
        #[cfg(feature = "icons")]
        None,
    )
}

/// Spawn a toolbar row with catalogue icons coloured from the active theme.
#[cfg(feature = "icons")]
pub fn spawn_toolbar_row_with_icons(
    commands: &mut Commands,
    left: impl IntoIterator<Item = ToolbarItem>,
    right: impl IntoIterator<Item = ToolbarItem>,
    icons: &IconSet,
    theme: &UiTheme,
) -> ToolbarRowEntities {
    spawn_toolbar_row_with_icons_aligned(
        commands,
        ToolbarAlignment::Edges,
        left,
        right,
        icons,
        theme,
    )
}

/// Spawn a toolbar row with explicit group placement and themed catalogue icons.
#[cfg(feature = "icons")]
pub fn spawn_toolbar_row_with_icons_aligned(
    commands: &mut Commands,
    alignment: ToolbarAlignment,
    left: impl IntoIterator<Item = ToolbarItem>,
    right: impl IntoIterator<Item = ToolbarItem>,
    icons: &IconSet,
    theme: &UiTheme,
) -> ToolbarRowEntities {
    spawn_toolbar_row_inner(commands, alignment, left, right, Some((icons, theme)))
}

fn spawn_toolbar_row_inner(
    commands: &mut Commands,
    alignment: ToolbarAlignment,
    left: impl IntoIterator<Item = ToolbarItem>,
    right: impl IntoIterator<Item = ToolbarItem>,
    #[cfg(feature = "icons")] icon_resources: Option<(&IconSet, &UiTheme)>,
) -> ToolbarRowEntities {
    let left = spawn_toolbar_group(
        commands,
        left,
        #[cfg(feature = "icons")]
        icon_resources,
    );
    let right = spawn_toolbar_group(
        commands,
        right,
        #[cfg(feature = "icons")]
        icon_resources,
    );
    let left_root = toolbar_group_root(commands, &left, alignment);
    let right_root = toolbar_group_root(commands, &right, alignment);
    // DCS top controls are absolute, higher-z overlays; the row must reserve
    // their full width or its outer buttons paint underneath them.
    let root = commands
        .spawn((
            Node {
                width: percent(100),
                min_width: px(0),
                flex_direction: FlexDirection::Row,
                align_items: AlignItems::Center,
                justify_content: match alignment {
                    ToolbarAlignment::Edges => JustifyContent::SpaceBetween,
                    ToolbarAlignment::Centre => JustifyContent::Center,
                },
                column_gap: px(12),
                padding: UiRect::horizontal(px(DCS_TOOLBAR_SAFE_PADDING_PX)),
                overflow: match alignment {
                    ToolbarAlignment::Edges => Overflow::DEFAULT,
                    ToolbarAlignment::Centre => Overflow::clip_x(),
                },
                overflow_clip_margin: match alignment {
                    ToolbarAlignment::Edges => OverflowClipMargin::DEFAULT,
                    ToolbarAlignment::Centre => OverflowClipMargin::content_box(),
                },
                ..default()
            },
            ThemeBackgroundColor(tokens::PANEL),
        ))
        .add_children(&[left_root, right_root])
        .id();
    ToolbarRowEntities { root, left, right }
}

fn spawn_toolbar_group(
    commands: &mut Commands,
    items: impl IntoIterator<Item = ToolbarItem>,
    #[cfg(feature = "icons")] icon_resources: Option<(&IconSet, &UiTheme)>,
) -> Vec<Entity> {
    items
        .into_iter()
        .map(|item| match item {
            ToolbarItem::Entity(entity) => entity,
            ToolbarItem::Button(button) => spawn_toolbar_button(
                commands,
                button,
                #[cfg(feature = "icons")]
                icon_resources,
            ),
        })
        .collect()
}

fn toolbar_group_root(
    commands: &mut Commands,
    children: &[Entity],
    alignment: ToolbarAlignment,
) -> Entity {
    commands
        .spawn(Node {
            min_width: px(0),
            flex_shrink: match alignment {
                ToolbarAlignment::Edges => 1.0,
                ToolbarAlignment::Centre => 0.0,
            },
            flex_direction: FlexDirection::Row,
            align_items: AlignItems::Center,
            column_gap: px(6),
            ..default()
        })
        .add_children(children)
        .id()
}

fn spawn_toolbar_button(
    commands: &mut Commands,
    button: ToolbarButtonDef,
    #[cfg(feature = "icons")] icon_resources: Option<(&IconSet, &UiTheme)>,
) -> Entity {
    let mut accessibility = accesskit::Node::new(Role::Button);
    accessibility.set_label(button.label.as_ref());
    let entity = commands
        .spawn((
            Button,
            TabIndex(0),
            AccessibilityNode::from(accessibility),
            Name::new(button.label.to_string()),
            Node {
                min_width: if button.show_label { px(0) } else { px(34) },
                height: px(30),
                padding: if button.show_label {
                    UiRect::axes(px(10), px(5))
                } else {
                    UiRect::ZERO
                },
                border_radius: BorderRadius::all(px(4)),
                column_gap: px(6),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                overflow: if button.show_label {
                    Overflow::clip_x()
                } else {
                    Overflow::DEFAULT
                },
                overflow_clip_margin: OverflowClipMargin::border_box(),
                ..default()
            },
            ThemeBackgroundColor(tokens::CONTROL),
        ))
        .id();

    let mut children = Vec::new();
    #[cfg(feature = "icons")]
    if let (Some(icon), Some((icons, theme))) = (button.icon, icon_resources) {
        // `spawn_icon` pins its own `min_width`, so the icon cannot shrink; what
        // it needs protection from is being clipped, which the label host below
        // provides by absorbing the overflow before it becomes negative free
        // space that centring would push out through the button's clip.
        children.push(spawn_icon(commands, icons, theme, icon, 17.0, tokens::TEXT));
    }
    if button.show_label {
        let text = commands
            .spawn((
                Text::new(button.label.to_string()),
                TextFont::from_font_size(13.0),
                TextLayout::no_wrap(),
                ThemeTextColor(tokens::TEXT),
            ))
            .id();
        // The label absorbs the overflow, not the button: clipping the whole
        // button would centre an over-wide icon+label pair and cut the icon's
        // leading edge off. A shrinkable clipping host truncates only the text.
        children.push(
            commands
                .spawn(Node {
                    min_width: px(0),
                    flex_shrink: 1.0,
                    overflow: Overflow::clip_x(),
                    overflow_clip_margin: OverflowClipMargin::border_box(),
                    ..default()
                })
                .add_child(text)
                .id(),
        );
    }
    commands.entity(entity).add_children(&children);
    #[cfg(feature = "actions")]
    if let Some(action) = button.action {
        commands.entity(entity).insert(ToolbarActionButton(action));
    }
    entity
}

/// Mutable value behind one shared single-line status text widget.
#[derive(Component, Clone, Debug, Default, PartialEq, Eq)]
pub struct StatusText(pub String);

impl StatusText {
    pub fn new(text: impl Into<String>) -> Self {
        Self(text.into())
    }

    pub fn set(&mut self, text: impl Into<String>) {
        self.0 = text.into();
    }
}

/// Root and primary text entity of a shared status bar.
pub struct StatusBarEntities {
    pub root: Entity,
    pub text: Entity,
}

/// Spawn one themed status text entity for embedding in a status bar or toolbar.
pub fn spawn_status_text(commands: &mut Commands, initial: impl Into<String>) -> Entity {
    let initial = initial.into();
    commands
        .spawn((
            Text::new(initial.clone()),
            TextFont::from_font_size(13.0),
            ThemeTextColor(tokens::TEXT_DIM),
            StatusText(initial),
        ))
        .id()
}

/// Spawn a themed single-line status bar.
pub fn spawn_status_bar(commands: &mut Commands, initial: impl Into<String>) -> StatusBarEntities {
    let text = spawn_status_text(commands, initial);
    let root = commands
        .spawn((
            Node {
                width: percent(100),
                min_height: px(28),
                min_width: px(0),
                flex_direction: FlexDirection::Row,
                align_items: AlignItems::Center,
                column_gap: px(18),
                padding: UiRect::axes(px(9), px(4)),
                ..default()
            },
            ThemeBackgroundColor(tokens::PANEL),
        ))
        .add_child(text)
        .id();
    StatusBarEntities { root, text }
}

/// Shared toolbar/status behaviour.
pub struct ChromePlugin;

impl Plugin for ChromePlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, sync_status_text);
        #[cfg(feature = "actions")]
        app.add_systems(Update, sync_toolbar_action_enabled);
    }
}

fn sync_status_text(mut texts: Query<(&StatusText, &mut Text), Changed<StatusText>>) {
    for (status, mut text) in &mut texts {
        if text.0 != status.0 {
            text.0.clone_from(&status.0);
        }
    }
}

#[cfg(feature = "actions")]
fn sync_toolbar_action_enabled(
    registry: Option<Res<ActionRegistryResource>>,
    buttons: Query<(Entity, &ToolbarActionButton, Has<InteractionDisabled>)>,
    mut commands: Commands,
) {
    let Some(registry) = registry else {
        return;
    };
    for (entity, action, disabled) in &buttons {
        let should_disable = registry.registry().is_enabled(action.0) != Some(true);
        if should_disable != disabled {
            if should_disable {
                commands.entity(entity).insert(InteractionDisabled);
            } else {
                commands.entity(entity).remove::<InteractionDisabled>();
            }
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
    use bevy::ecs::hierarchy::Children;
    use bevy::ecs::system::RunSystemOnce;
    use bevy::ecs::world::CommandQueue;
    use bevy::image::{ImagePlugin, TextureAtlasPlugin};
    use bevy::input::InputPlugin;
    use bevy::math::UVec2;
    use bevy::mesh::MeshPlugin;
    use bevy::picking::PickingPlugin;
    use bevy::prelude::MinimalPlugins;
    use bevy::text::{LineBreak, TextLayoutInfo, TextPlugin};
    use bevy::transform::TransformPlugin;
    use bevy::ui::{CalculatedClip, ComputedNode, UiGlobalTransform, UiPlugin};

    #[test]
    fn changed_status_value_updates_rendered_text() {
        let mut app = App::new();
        app.add_plugins(ChromePlugin);
        let entity = app
            .world_mut()
            .spawn((StatusText::new("old"), Text::new("old")))
            .id();
        app.world_mut()
            .entity_mut(entity)
            .get_mut::<StatusText>()
            .unwrap()
            .set("new");
        app.update();
        assert_eq!(app.world().entity(entity).get::<Text>().unwrap().0, "new");
    }

    #[test]
    fn toolbar_places_items_in_distinct_groups() {
        let mut world = bevy::ecs::world::World::new();
        let left = world.spawn_empty().id();
        let right = world.spawn_empty().id();
        let row = world
            .run_system_once(move |mut commands: Commands| {
                spawn_toolbar_row(
                    &mut commands,
                    [ToolbarItem::Entity(left)],
                    [ToolbarItem::Entity(right)],
                )
            })
            .unwrap();
        assert_eq!(row.left, [left]);
        assert_eq!(row.right, [right]);
    }

    #[test]
    fn toolbar_alignment_preserves_edges_default_and_centres_inside_dcs_safe_area() {
        let mut world = bevy::ecs::world::World::new();
        let edges = world
            .run_system_once(|mut commands: Commands| spawn_toolbar_row(&mut commands, [], []))
            .unwrap();
        let centre = world
            .run_system_once(|mut commands: Commands| {
                spawn_toolbar_row_aligned(&mut commands, ToolbarAlignment::Centre, [], [])
            })
            .unwrap();

        let edges_node = world.entity(edges.root).get::<Node>().unwrap();
        assert_eq!(edges_node.justify_content, JustifyContent::SpaceBetween);
        assert_eq!(
            edges_node.padding,
            UiRect::horizontal(px(DCS_TOOLBAR_SAFE_PADDING_PX))
        );
        assert_eq!(edges_node.overflow, Overflow::DEFAULT);

        let centre_node = world.entity(centre.root).get::<Node>().unwrap();
        assert_eq!(centre_node.min_width, px(0));
        assert_eq!(centre_node.justify_content, JustifyContent::Center);
        assert_eq!(centre_node.column_gap, px(12));
        assert_eq!(
            centre_node.padding,
            UiRect::horizontal(px(DCS_TOOLBAR_SAFE_PADDING_PX))
        );
        assert_eq!(centre_node.overflow, Overflow::clip_x());
    }

    #[test]
    fn toolbar_button_labels_never_wrap() {
        let mut world = bevy::ecs::world::World::new();
        let row = world
            .run_system_once(|mut commands: Commands| {
                spawn_toolbar_row(
                    &mut commands,
                    [ToolbarButtonDef::new("Refresh mesh").into()],
                    [],
                )
            })
            .unwrap();
        let label = find_label(&world, row.left[0]).unwrap();
        assert!(matches!(
            world.get::<TextLayout>(label).unwrap().linebreak,
            LineBreak::NoWrap
        ));
    }

    #[test]
    fn long_toolbar_label_is_clipped_to_its_button() {
        let mut app = layout_test_app();
        let right = fixed_toolbar_items(app.world_mut(), 1);
        let mut queue = CommandQueue::default();
        let row = {
            let mut commands = Commands::new(&mut queue, app.world());
            spawn_toolbar_row(
                &mut commands,
                [ToolbarButtonDef::with_dynamic_label("x".repeat(200)).into()],
                right,
            )
        };
        queue.apply(app.world_mut());
        app.world_mut()
            .spawn(Node {
                width: px(300),
                height: px(42),
                ..default()
            })
            .add_child(row.root);
        app.world_mut().run_schedule(PostUpdate);

        let world = app.world();
        let button = row.left[0];
        let label = find_label(world, button).unwrap();
        let button_computed = world.get::<ComputedNode>(button).unwrap();
        let button_transform = world.get::<UiGlobalTransform>(button).unwrap();
        let label_layout = world.get::<TextLayoutInfo>(label).unwrap();
        let label_clip = world.get::<CalculatedClip>(label).unwrap().clip;
        let button_min = button_transform.translation.x - button_computed.size().x / 2.0;
        let button_max = button_transform.translation.x + button_computed.size().x / 2.0;

        // The text really is wider than its button, so the test would notice if
        // the button simply grew to fit instead of the label being clipped.
        assert!(label_layout.size.x > button_computed.size().x);
        // The clip comes from the label's own shrinkable host, which sits
        // inside the button's padding — so it must be within the button, not
        // merely coincident with it.
        assert!(label_clip.min.x >= button_min - 0.5);
        assert!(label_clip.max.x <= button_max + 0.5);
        assert!(label_clip.max.x - label_clip.min.x <= button_computed.size().x + 0.5);
    }

    /// The label sits inside a shrinkable clipping host, so it is a grandchild
    /// of the button rather than a direct child.
    fn find_label(world: &bevy::ecs::world::World, root: Entity) -> Option<Entity> {
        if world.get::<Text>(root).is_some() {
            return Some(root);
        }
        world
            .get::<Children>(root)?
            .iter()
            .copied()
            .find_map(|child| find_label(world, child))
    }

    #[cfg(feature = "icons")]
    #[test]
    fn long_label_never_clips_the_button_icon() {
        // Clipping the whole button would centre an over-wide icon+label pair
        // and cut the icon's leading edge — the same class of defect as a
        // toolbar button hidden under the DCS controls.
        let mut app = layout_test_app();
        let theme = UiTheme::default();
        let icons = IconSet::placeholder_for_test(&[Icon::Info]);
        let mut queue = CommandQueue::default();
        let row = {
            let mut commands = Commands::new(&mut queue, app.world());
            spawn_toolbar_row_with_icons(
                &mut commands,
                [ToolbarButtonDef::with_dynamic_label("x".repeat(200))
                    .with_icon(Icon::Info)
                    .into()],
                [],
                &icons,
                &theme,
            )
        };
        queue.apply(app.world_mut());
        app.world_mut()
            .spawn(Node {
                width: px(300),
                height: px(42),
                ..default()
            })
            .add_child(row.root);
        app.world_mut().run_schedule(PostUpdate);

        let world = app.world();
        let button = row.left[0];
        let icon = world.get::<Children>(button).unwrap()[0];
        let icon_computed = world.get::<ComputedNode>(icon).unwrap();
        let icon_transform = world.get::<UiGlobalTransform>(icon).unwrap();
        let icon_min = icon_transform.translation.x - icon_computed.size().x / 2.0;
        let icon_max = icon_transform.translation.x + icon_computed.size().x / 2.0;

        assert!(icon_computed.size().x >= 17.0, "icon was squeezed narrower");
        if let Some(clip) = world.get::<CalculatedClip>(icon) {
            assert!(
                icon_min >= clip.clip.min.x - 0.5,
                "icon's left edge clipped"
            );
            assert!(
                icon_max <= clip.clip.max.x + 0.5,
                "icon's right edge clipped"
            );
        }
    }

    #[derive(Debug)]
    struct ToolbarLayoutGeometry {
        row_centre: f32,
        clip_min: f32,
        clip_max: f32,
        left_min: f32,
        right_max: f32,
        left_width: f32,
        right_width: f32,
    }

    fn layout_test_app() -> App {
        const TARGET_WIDTH: u32 = 800;
        const TARGET_HEIGHT: u32 = 100;

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
                        physical_size: UVec2::new(TARGET_WIDTH, TARGET_HEIGHT),
                        scale_factor: 1.0,
                    }),
                    ..default()
                },
                viewport: Some(Viewport {
                    physical_size: UVec2::new(TARGET_WIDTH, TARGET_HEIGHT),
                    ..default()
                }),
                ..default()
            },
        ));
        app.finish();
        app.cleanup();
        app
    }

    fn fixed_toolbar_items(world: &mut bevy::ecs::world::World, count: usize) -> Vec<ToolbarItem> {
        (0..count)
            .map(|_| {
                ToolbarItem::Entity(
                    world
                        .spawn(Node {
                            width: px(34),
                            height: px(30),
                            flex_shrink: 0.0,
                            ..default()
                        })
                        .id(),
                )
            })
            .collect()
    }

    fn centred_toolbar_geometry(width: f32) -> ToolbarLayoutGeometry {
        let mut app = layout_test_app();
        let left = fixed_toolbar_items(app.world_mut(), 6);
        let right = fixed_toolbar_items(app.world_mut(), 4);
        let mut queue = CommandQueue::default();
        let row = {
            let mut commands = Commands::new(&mut queue, app.world());
            spawn_toolbar_row_aligned(&mut commands, ToolbarAlignment::Centre, left, right)
        };
        queue.apply(app.world_mut());
        let parent = app
            .world_mut()
            .spawn(Node {
                width: px(width),
                height: px(42),
                ..default()
            })
            .add_child(row.root)
            .id();
        app.world_mut().run_schedule(PostUpdate);

        let world = app.world();
        let group_roots = world.get::<Children>(row.root).unwrap();
        let left_root = group_roots[0];
        let right_root = group_roots[1];
        let row_transform = world.get::<UiGlobalTransform>(row.root).unwrap();
        let left_computed = world.get::<ComputedNode>(left_root).unwrap();
        let left_transform = world.get::<UiGlobalTransform>(left_root).unwrap();
        let right_computed = world.get::<ComputedNode>(right_root).unwrap();
        let right_transform = world.get::<UiGlobalTransform>(right_root).unwrap();
        let left_clip = world.get::<CalculatedClip>(left_root).unwrap().clip;
        let right_clip = world.get::<CalculatedClip>(right_root).unwrap().clip;

        assert_eq!(left_clip, right_clip);
        assert_eq!(world.get::<ComputedNode>(parent).unwrap().size().x, width);
        ToolbarLayoutGeometry {
            row_centre: row_transform.translation.x,
            clip_min: left_clip.min.x,
            clip_max: left_clip.max.x,
            left_min: left_transform.translation.x - left_computed.size().x / 2.0,
            right_max: right_transform.translation.x + right_computed.size().x / 2.0,
            left_width: left_computed.size().x,
            right_width: right_computed.size().x,
        }
    }

    #[test]
    fn centred_toolbar_clips_symmetrically_at_narrow_safe_boundaries() {
        let layouts = [
            centred_toolbar_geometry(544.0),
            centred_toolbar_geometry(420.0),
        ];

        for layout in &layouts {
            assert!(layout.left_min < layout.clip_min);
            assert!(layout.right_max > layout.clip_max);
            assert!(
                ((layout.clip_min - layout.left_min) - (layout.right_max - layout.clip_max)).abs()
                    <= 1.0,
                "{layout:?}"
            );
            assert!(
                ((layout.row_centre - layout.left_min) - (layout.right_max - layout.row_centre))
                    .abs()
                    <= 1.0,
                "{layout:?}"
            );
        }
        assert_eq!(layouts[0].left_width, layouts[1].left_width);
        assert_eq!(layouts[0].right_width, layouts[1].right_width);
    }
}
