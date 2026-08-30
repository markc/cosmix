//! Reusable flat tree-view behaviour for native CTK applications.
//!
//! Applications keep ownership of row content and layout. CTK owns the
//! disclosure control, expanded state and descendant visibility. Keeping rows
//! flat is deliberate: file browsers and inspectors can align trailing columns
//! while indenting only the disclosure/name cell.

use bevy::app::{App, Plugin};
use bevy::ecs::entity::Entity;
use bevy::ecs::event::EntityEvent;
use bevy::ecs::hierarchy::ChildOf;
use bevy::ecs::observer::On;
use bevy::feathers::theme::{ThemeTextColor, UiTheme};
use bevy::input_focus::tab_navigation::TabIndex;
use bevy::picking::events::{Click, Pointer};
use bevy::picking::hover::Hovered;
use bevy::picking::Pickable;
use bevy::prelude::*;
use bevy::ui::{px, UiRect};
use bevy::ui_widgets::Activate;

use crate::theme::{ctk_color, tokens};

/// Marker for the root that owns a set of flat [`TreeItem`] rows.
#[derive(Component)]
pub struct TreeView;

/// Expansion metadata stored on an application-owned row entity.
#[derive(Component, Clone, Copy, Debug)]
pub struct TreeItem {
    view: Entity,
    parent: Option<Entity>,
    expanded: bool,
    expandable: bool,
}

impl TreeItem {
    pub fn branch(view: Entity, parent: Option<Entity>, expanded: bool) -> Self {
        Self {
            view,
            parent,
            expanded,
            expandable: true,
        }
    }

    pub fn leaf(view: Entity, parent: Option<Entity>) -> Self {
        Self {
            view,
            parent,
            expanded: false,
            expandable: false,
        }
    }

    pub fn view(&self) -> Entity {
        self.view
    }

    pub fn parent(&self) -> Option<Entity> {
        self.parent
    }

    pub fn is_expanded(&self) -> bool {
        self.expanded
    }

    pub fn is_expandable(&self) -> bool {
        self.expandable
    }
}

/// Emitted on the item row after its disclosure state changes.
#[derive(EntityEvent, Clone, Copy, Debug, PartialEq, Eq)]
pub struct TreeViewChanged {
    #[event_target]
    pub item: Entity,
    pub expanded: bool,
}

#[derive(Component)]
pub struct TreeDisclosure {
    item: Entity,
    collapsed_visual: Option<Entity>,
    expanded_visual: Option<Entity>,
    label: Option<Entity>,
}

#[derive(Component)]
struct TreeDisclosureLabel;

/// Install disclosure behaviour and descendant visibility updates.
pub struct TreeViewPlugin;

impl Plugin for TreeViewPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<UiTheme>()
            .add_observer(on_disclosure_activated)
            .add_observer(on_disclosure_clicked)
            .add_systems(Update, paint_disclosures);
    }
}

/// Spawn a standard CTK tree root. Rows remain direct children of this entity.
pub fn spawn_tree_view(commands: &mut Commands) -> Entity {
    commands
        .spawn((
            Node {
                width: percent(100),
                flex_shrink: 0.0,
                flex_direction: FlexDirection::Column,
                ..default()
            },
            TreeView,
        ))
        .id()
}

/// Re-evaluate every row in `view` after an application has lazily inserted
/// or rebuilt descendants.
pub fn sync_tree_view(commands: &mut Commands, view: Entity) {
    commands.queue(move |world: &mut World| sync_tree_visibility(world, view));
}

/// Spawn a disclosure control for `item` at the supplied logical depth.
///
/// The returned entity always occupies the same slot. Leaves use a non-button
/// spacer so names align with branch rows.
pub fn spawn_tree_disclosure(
    commands: &mut Commands,
    item: Entity,
    depth: usize,
    expandable: bool,
    expanded: bool,
) -> Entity {
    spawn_tree_disclosure_inner(commands, item, depth, expandable, expanded, None)
}

/// Spawn a disclosure using application-supplied collapsed and expanded icon
/// entities. CTK switches their display state; the application remains free to
/// use SVG, raster or code-drawn icons.
pub fn spawn_tree_disclosure_with_icons(
    commands: &mut Commands,
    item: Entity,
    depth: usize,
    expandable: bool,
    expanded: bool,
    collapsed_icon: Entity,
    expanded_icon: Entity,
) -> Entity {
    spawn_tree_disclosure_inner(
        commands,
        item,
        depth,
        expandable,
        expanded,
        Some((collapsed_icon, expanded_icon)),
    )
}

fn spawn_tree_disclosure_inner(
    commands: &mut Commands,
    item: Entity,
    depth: usize,
    expandable: bool,
    expanded: bool,
    visuals: Option<(Entity, Entity)>,
) -> Entity {
    let indent = (depth.min(64) as f32) * 14.0;
    let mut entity = commands.spawn((
        Node {
            width: px(18),
            min_width: px(18),
            height: px(20),
            margin: UiRect::left(px(indent)),
            justify_content: JustifyContent::Center,
            align_items: AlignItems::Center,
            border_radius: BorderRadius::all(px(2)),
            ..default()
        },
        BackgroundColor(Color::NONE),
    ));
    if expandable {
        let (collapsed_visual, expanded_visual) = visuals.unzip();
        entity.insert((
            Button,
            Pickable::default(),
            Hovered::default(),
            TabIndex(0),
            TreeDisclosure {
                item,
                collapsed_visual,
                expanded_visual,
                label: None,
            },
            Name::new("Toggle tree item"),
        ));
    }
    let disclosure = entity.id();
    if expandable && visuals.is_none() {
        let label = commands
            .spawn((
                Text::new(disclosure_glyph(expanded)),
                TextFont::from_font_size(14.0),
                ThemeTextColor(tokens::TEXT_DIM),
                TreeDisclosureLabel,
            ))
            .id();
        commands
            .entity(disclosure)
            .add_child(label)
            .insert(TreeDisclosure {
                item,
                collapsed_visual: None,
                expanded_visual: None,
                label: Some(label),
            });
    } else if let Some((collapsed, expanded)) = visuals {
        commands
            .entity(disclosure)
            .add_children(&[collapsed, expanded]);
    }
    disclosure
}

fn disclosure_glyph(expanded: bool) -> &'static str {
    if expanded {
        "v"
    } else {
        ">"
    }
}

fn on_disclosure_activated(
    activated: On<Activate>,
    disclosures: Query<&TreeDisclosure>,
    mut items: Query<&mut TreeItem>,
    mut commands: Commands,
) {
    toggle_disclosure(activated.entity, &disclosures, &mut items, &mut commands);
}

fn on_disclosure_clicked(
    mut click: On<Pointer<Click>>,
    disclosures: Query<&TreeDisclosure>,
    parents: Query<&ChildOf>,
    mut items: Query<&mut TreeItem>,
    mut commands: Commands,
) {
    let mut entity = click.original_event_target();
    if disclosures.contains(entity) {
        return;
    }
    loop {
        if disclosures.contains(entity) {
            click.propagate(false);
            toggle_disclosure(entity, &disclosures, &mut items, &mut commands);
            return;
        }
        let Ok(parent) = parents.get(entity) else {
            return;
        };
        entity = parent.parent();
    }
}

fn toggle_disclosure(
    entity: Entity,
    disclosures: &Query<&TreeDisclosure>,
    items: &mut Query<&mut TreeItem>,
    commands: &mut Commands,
) {
    let Ok(disclosure) = disclosures.get(entity) else {
        return;
    };
    let (view, expanded) = {
        let Ok(mut item) = items.get_mut(disclosure.item) else {
            return;
        };
        if !item.expandable {
            return;
        }
        item.expanded = !item.expanded;
        (item.view, item.expanded)
    };

    commands.trigger(TreeViewChanged {
        item: disclosure.item,
        expanded,
    });
    sync_tree_view(commands, view);
}

fn sync_tree_visibility(world: &mut World, view: Entity) {
    let snapshot: std::collections::HashMap<Entity, TreeItem> = world
        .query::<(Entity, &TreeItem)>()
        .iter(world)
        .filter(|(_, item)| item.view == view)
        .map(|(entity, item)| (entity, *item))
        .collect();

    for (entity, item) in &snapshot {
        let visible = ancestors_expanded(*item, &snapshot);
        if let Some(mut node) = world.get_mut::<Node>(*entity) {
            node.display = if visible {
                Display::Flex
            } else {
                Display::None
            };
        }
    }
}

fn ancestors_expanded(item: TreeItem, items: &std::collections::HashMap<Entity, TreeItem>) -> bool {
    let mut parent = item.parent;
    let mut remaining = items.len();
    while let Some(entity) = parent {
        if remaining == 0 {
            return false;
        }
        remaining -= 1;
        let Some(parent_item) = items.get(&entity) else {
            return false;
        };
        if !parent_item.expanded {
            return false;
        }
        parent = parent_item.parent;
    }
    true
}

fn paint_disclosures(
    theme: Res<UiTheme>,
    disclosures: Query<(Entity, &TreeDisclosure, &Hovered)>,
    items: Query<&TreeItem>,
    mut labels: Query<&mut Text, With<TreeDisclosureLabel>>,
    mut nodes: Query<&mut Node>,
    mut backgrounds: Query<&mut BackgroundColor>,
) {
    for (entity, disclosure, hovered) in &disclosures {
        let Ok(item) = items.get(disclosure.item) else {
            continue;
        };
        if let Ok(mut background) = backgrounds.get_mut(entity) {
            background.0 = if hovered.get() {
                ctk_color(&theme, &tokens::ROW_HOVER)
            } else {
                Color::NONE
            };
        }
        if let Some(label_entity) = disclosure.label {
            if let Ok(mut label) = labels.get_mut(label_entity) {
                label.0 = disclosure_glyph(item.expanded).into();
            }
        }
        if let (Some(collapsed), Some(expanded)) =
            (disclosure.collapsed_visual, disclosure.expanded_visual)
        {
            if let Ok(mut node) = nodes.get_mut(collapsed) {
                node.display = if item.expanded {
                    Display::None
                } else {
                    Display::Flex
                };
            }
            if let Ok(mut node) = nodes.get_mut(expanded) {
                node.display = if item.expanded {
                    Display::Flex
                } else {
                    Display::None
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disclosure_activate_toggles_state_and_descendant_visibility() {
        let mut app = App::new();
        app.add_plugins(TreeViewPlugin);

        let world = app.world_mut();
        let view = world.spawn((Node::default(), TreeView)).id();
        let parent = world
            .spawn((Node::default(), TreeItem::branch(view, None, false)))
            .id();
        let child = world
            .spawn((Node::default(), TreeItem::leaf(view, Some(parent))))
            .id();
        let disclosure = world
            .spawn((
                Node::default(),
                Button,
                Pickable::default(),
                Hovered::default(),
                TreeDisclosure {
                    item: parent,
                    collapsed_visual: None,
                    expanded_visual: None,
                    label: None,
                },
            ))
            .id();

        world.trigger(Activate { entity: disclosure });
        world.flush();

        assert!(world.get::<TreeItem>(parent).unwrap().is_expanded());
        assert_eq!(world.get::<Node>(child).unwrap().display, Display::Flex);

        world.trigger(Activate { entity: disclosure });
        world.flush();

        assert!(!world.get::<TreeItem>(parent).unwrap().is_expanded());
        assert_eq!(world.get::<Node>(child).unwrap().display, Display::None);
    }

    #[test]
    fn collapsed_ancestor_hides_descendants() {
        let view = Entity::from_raw_u32(1).unwrap();
        let parent = Entity::from_raw_u32(2).unwrap();
        let child = Entity::from_raw_u32(3).unwrap();
        let mut items = std::collections::HashMap::new();
        items.insert(parent, TreeItem::branch(view, None, false));
        let child_item = TreeItem::leaf(view, Some(parent));
        items.insert(child, child_item);
        assert!(!ancestors_expanded(child_item, &items));
    }

    #[test]
    fn expanded_chain_shows_descendant() {
        let view = Entity::from_raw_u32(1).unwrap();
        let parent = Entity::from_raw_u32(2).unwrap();
        let child_item = TreeItem::leaf(view, Some(parent));
        let items = std::collections::HashMap::from([
            (parent, TreeItem::branch(view, None, true)),
            (Entity::from_raw_u32(3).unwrap(), child_item),
        ]);
        assert!(ancestors_expanded(child_item, &items));
    }

    #[test]
    fn missing_or_cyclic_parent_is_hidden() {
        let view = Entity::from_raw_u32(1).unwrap();
        let first = Entity::from_raw_u32(2).unwrap();
        let second = Entity::from_raw_u32(3).unwrap();
        let cyclic = TreeItem::branch(view, Some(second), true);
        let items = std::collections::HashMap::from([
            (first, cyclic),
            (second, TreeItem::branch(view, Some(first), true)),
        ]);
        assert!(!ancestors_expanded(cyclic, &items));
        assert!(!ancestors_expanded(
            TreeItem::leaf(view, Some(Entity::from_raw_u32(9).unwrap())),
            &items
        ));
    }
}
