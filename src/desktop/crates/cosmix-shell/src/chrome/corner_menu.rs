//! Corner menu presentation shared with the native host. No mode mutation here.
use crate::core::{Corner, Edge, OutputKey, PanelInput, PanelMode};
use crate::runtime::ShellCommandKind;
use bevy::prelude::*;
use bevy::ui::{percent, px};
use ctk::theme::tokens;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MenuExtra {
    pub label: String,
    pub target: String,
    pub verb: String,
    pub args: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MenuAction {
    Mode(PanelMode),
    Extra(MenuExtra),
    /// Built-in "Edit panels…" (scene-editor plan §4.3 Q1): frame, not
    /// content, so it is never a `conf.mix` extra. It sends
    /// `scenes.editor.open` with body exactly `{"safe":true}` to
    /// `env("SCENES_SERVICE","scenes")`; the menu lists it after the mode
    /// items on every corner. Declared in Stage S; Q1 wires it.
    EditPanels,
}

#[derive(Clone, Debug)]
pub struct MenuItem {
    pub label: String,
    pub checked: bool,
    pub action: MenuAction,
}

impl MenuItem {
    pub fn command(&self, edge: Edge) -> Option<ShellCommandKind> {
        if self.checked {
            return None;
        }
        match self.action {
            MenuAction::Mode(mode) => Some(ShellCommandKind::Panel {
                edge,
                input: PanelInput::SetMode(mode),
            }),
            MenuAction::Extra(_) | MenuAction::EditPanels => None,
        }
    }
}

pub fn menu_items(mode: PanelMode, extras: &[MenuExtra]) -> Vec<MenuItem> {
    [
        ("Pin", PanelMode::Pinned),
        ("Dock", PanelMode::Docked),
        ("Hide", PanelMode::Hidden),
    ]
    .into_iter()
    .map(|(label, value)| MenuItem {
        label: label.into(),
        checked: mode == value,
        action: MenuAction::Mode(value),
    })
    .chain(extras.iter().cloned().map(|extra| MenuItem {
        label: extra.label.clone(),
        checked: false,
        action: MenuAction::Extra(extra),
    }))
    .collect()
}

/// Snapshot supplied by the hook. Reopening reads the latest accepted config.
#[derive(Resource, Clone)]
pub struct CornerMenuRequest {
    pub output: OutputKey,
    pub corner: Corner,
    pub items: Vec<MenuItem>,
}

/// App-owned Bus dispatch, called only for a user-selected extra item.
#[derive(Resource, Clone, Copy)]
pub struct CornerMenuExtraHook(pub fn(&mut World, MenuExtra));

pub const ROW_HEIGHT: f32 = 32.0;
pub const MENU_WIDTH: f32 = 220.0;

/// Position rows inward from their owning corner. Label padding keeps text
/// away from the compositor-owned hotspot.
pub fn menu_origin(corner: Corner, size: Vec2, rows: usize) -> Vec2 {
    let right = matches!(corner, Corner::TopRight | Corner::BottomRight);
    let bottom = matches!(corner, Corner::BottomLeft | Corner::BottomRight);
    Vec2::new(
        if right {
            (size.x - MENU_WIDTH).max(0.0)
        } else {
            0.0
        },
        if bottom {
            (size.y - ROW_HEIGHT * rows as f32).max(0.0)
        } else {
            0.0
        },
    )
}

pub fn hit_row(position: Vec2, origin: Vec2, rows: usize) -> Option<usize> {
    let p = position - origin;
    (p.x >= 0.0 && p.x < MENU_WIDTH && p.y >= 0.0 && p.y < ROW_HEIGHT * rows as f32)
        .then(|| (p.y / ROW_HEIGHT) as usize)
}

/// The transparent root catches click-away on this output. The native host
/// owns the input and keyboard lifecycle; rows use the panel chrome palette.
pub fn spawn_menu(
    world: &mut World,
    mount: Entity,
    request: &CornerMenuRequest,
    size: Vec2,
) -> Vec<Entity> {
    let origin = menu_origin(request.corner, size, request.items.len());
    let popup = world
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                left: px(origin.x),
                top: px(origin.y),
                width: px(MENU_WIDTH),
                flex_direction: FlexDirection::Column,
                ..default()
            },
            bevy::feathers::theme::ThemeBackgroundColor(tokens::PANEL),
        ))
        .id();
    world.entity_mut(mount).add_child(popup);
    request
        .items
        .iter()
        .map(|item| {
            let row = world
                .spawn((
                    Node {
                        width: percent(100),
                        height: px(ROW_HEIGHT),
                        min_height: px(ROW_HEIGHT),
                        align_items: AlignItems::Center,
                        padding: UiRect::horizontal(px(12)),
                        ..default()
                    },
                    bevy::feathers::theme::ThemeBackgroundColor(tokens::PANEL),
                ))
                .id();
            if item.checked {
                world.entity_mut(row).insert(bevy::ui::InteractionDisabled);
            }
            let label = world
                .spawn((
                    Text::new(format!(
                        "{}{}",
                        if item.checked { "✓  " } else { "    " },
                        item.label
                    )),
                    TextFont::from_font_size(14.0),
                    ctk::theme::CtkTextRole::Ui,
                    bevy::feathers::theme::ThemeTextColor(if item.checked {
                        tokens::TEXT_DIM
                    } else {
                        tokens::TEXT
                    }),
                ))
                .id();
            world.entity_mut(row).add_child(label);
            world.entity_mut(popup).add_child(row);
            row
        })
        .collect()
}

pub fn highlight(world: &mut World, rows: &[Entity], selected: Option<usize>) {
    for (index, entity) in rows.iter().enumerate() {
        world
            .entity_mut(*entity)
            .insert(bevy::feathers::theme::ThemeBackgroundColor(
                if selected == Some(index) {
                    tokens::ROW_HOVER
                } else {
                    tokens::PANEL
                },
            ));
    }
}

/// Keep the checked/disabled row accurate when another mode command arrives
/// while the menu is open (including commands in the opening input batch).
pub fn refresh_mode(
    world: &mut World,
    rows: &[Entity],
    request: &mut CornerMenuRequest,
    mode: PanelMode,
) -> bool {
    let mut changed = false;
    for (item, row) in request.items.iter_mut().zip(rows) {
        let MenuAction::Mode(value) = item.action else {
            continue;
        };
        let checked = value == mode;
        if item.checked == checked {
            continue;
        }
        changed = true;
        item.checked = checked;
        if checked {
            world.entity_mut(*row).insert(bevy::ui::InteractionDisabled);
        } else {
            world
                .entity_mut(*row)
                .remove::<bevy::ui::InteractionDisabled>();
        }
        let label = world.get::<Children>(*row).unwrap()[0];
        world.entity_mut(label).insert((
            Text::new(format!(
                "{}{}",
                if checked { "✓  " } else { "    " },
                item.label
            )),
            bevy::feathers::theme::ThemeTextColor(if checked {
                tokens::TEXT_DIM
            } else {
                tokens::TEXT
            }),
        ));
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn menu_lists_pin_dock_hide_with_current_mode_disabled() {
        for mode in [PanelMode::Hidden, PanelMode::Pinned, PanelMode::Docked] {
            let extra = MenuExtra {
                label: "Tools".into(),
                target: "tools".into(),
                verb: "tools.open".into(),
                args: vec![],
            };
            let items = menu_items(mode, std::slice::from_ref(&extra));
            assert_eq!(
                items.iter().map(|i| i.label.as_str()).collect::<Vec<_>>(),
                ["Pin", "Dock", "Hide", "Tools"]
            );
            assert_eq!(items.iter().filter(|i| i.checked).count(), 1);
            let current = items.iter().find(|i| i.checked).unwrap();
            assert_eq!(current.action, MenuAction::Mode(mode));
            assert_eq!(current.command(Edge::Left), None);
            assert_eq!(items[3].action, MenuAction::Extra(extra));
        }
    }
    #[test]
    fn menu_choice_emits_setmode_command() {
        for edge in Edge::ALL {
            for current in [PanelMode::Hidden, PanelMode::Pinned, PanelMode::Docked] {
                for item in menu_items(current, &[]) {
                    if let MenuAction::Mode(mode) = item.action {
                        assert_eq!(
                            item.command(edge),
                            (mode != current).then_some(ShellCommandKind::Panel {
                                edge,
                                input: PanelInput::SetMode(mode),
                            })
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn menu_rows_render_checked_disabled_and_follow_mode_changes() {
        let mut world = World::new();
        let mount = world.spawn(Node::default()).id();
        let mut request = CornerMenuRequest {
            output: OutputKey::new("test-output").unwrap(),
            corner: Corner::BottomRight,
            items: menu_items(PanelMode::Hidden, &[]),
        };
        let rows = spawn_menu(&mut world, mount, &request, Vec2::new(1000.0, 800.0));
        for mode in [PanelMode::Hidden, PanelMode::Pinned, PanelMode::Docked] {
            refresh_mode(&mut world, &rows, &mut request, mode);
            for (row, item) in rows.iter().zip(&request.items) {
                assert_eq!(
                    world.get::<bevy::ui::InteractionDisabled>(*row).is_some(),
                    item.checked
                );
                let label = world.get::<Children>(*row).unwrap()[0];
                assert_eq!(
                    world.get::<Text>(label).unwrap().0.starts_with('✓'),
                    item.checked
                );
            }
        }
        let origin = menu_origin(request.corner, Vec2::new(1000.0, 800.0), 3);
        assert_eq!(
            hit_row(origin + Vec2::new(20.0, ROW_HEIGHT + 1.0), origin, 3),
            Some(1)
        );
        assert_eq!(hit_row(origin - Vec2::ONE, origin, 3), None);
    }
}
