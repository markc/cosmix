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
    /// A question to confirm first: choosing the extra opens a confirm step
    /// ([`confirm_items`]) instead of calling the verb, so a misclick does
    /// nothing. A human affordance of the menu only; the verb itself needs no
    /// confirmation.
    pub confirm: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MenuAction {
    Mode(PanelMode),
    Extra(MenuExtra),
    /// Built-in "Edit panels…" (scene-editor plan §4.3 Q1): frame, not
    /// content, so it is never a `conf.mix` extra. The app's
    /// [`CornerMenuActionHook`] sends `scenes.editor.open` with body exactly
    /// `{"safe":true}` to `env("SCENES_SERVICE","scenes")`; the menu lists
    /// it right after the mode items on every corner. Safe open toggles, so
    /// choosing it while the shipped editor is visible closes the editor.
    EditPanels,
    /// A row that does nothing when chosen: a confirm step's question (shown
    /// disabled) and its Cancel.
    Inert,
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
            MenuAction::Extra(_) | MenuAction::EditPanels | MenuAction::Inert => None,
        }
    }
}

/// Label of the built-in [`MenuAction::EditPanels`] row.
pub const EDIT_PANELS_LABEL: &str = "Edit panels…";

/// Pin, Dock, Hide, the built-in "Edit panels…", then the app's extras. The
/// built-in row does not depend on configuration: it is always present.
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
    .chain(std::iter::once(MenuItem {
        label: EDIT_PANELS_LABEL.into(),
        checked: false,
        action: MenuAction::EditPanels,
    }))
    .chain(extras.iter().cloned().map(|extra| MenuItem {
        label: extra.label.clone(),
        checked: false,
        action: MenuAction::Extra(extra),
    }))
    .collect()
}

/// Label of a confirm step's cancelling row.
pub const CANCEL_LABEL: &str = "Cancel";

/// A confirm step: the question (disabled), the confirming row, then Cancel.
/// Choosing the confirming row performs `action`; Cancel, Escape or a click
/// away does nothing.
pub fn confirm_items(question: &str, confirm_label: &str, action: MenuAction) -> Vec<MenuItem> {
    vec![
        MenuItem {
            label: question.into(),
            checked: true,
            action: MenuAction::Inert,
        },
        MenuItem {
            label: confirm_label.into(),
            checked: false,
            action,
        },
        MenuItem {
            label: CANCEL_LABEL.into(),
            checked: false,
            action: MenuAction::Inert,
        },
    ]
}

/// Snapshot supplied by the hook. Reopening reads the latest accepted config.
#[derive(Resource, Clone)]
pub struct CornerMenuRequest {
    pub output: OutputKey,
    pub corner: Corner,
    pub items: Vec<MenuItem>,
}

/// App-owned Bus dispatch, called for a user-selected item that is not a mode
/// ([`MenuAction::Extra`] or [`MenuAction::EditPanels`]); mode items become
/// shell commands through [`MenuItem::command`] instead.
#[derive(Resource, Clone, Copy)]
pub struct CornerMenuActionHook(pub fn(&mut World, MenuAction));

pub const ROW_HEIGHT: f32 = 32.0;
pub const MENU_WIDTH: f32 = 220.0;
/// Characters a confirm question wraps at in one row line (a conservative
/// estimate for 14 px text in the menu width), and the most lines it may use.
pub const QUESTION_LINE_CHARS: usize = 24;
pub const QUESTION_MAX_LINES: usize = 3;
/// The longest confirm question: config refuses longer ones, so a question
/// always fits the rows reserved for it.
pub const QUESTION_MAX_CHARS: usize = QUESTION_LINE_CHARS * QUESTION_MAX_LINES;

/// A confirm step's question: a disabled [`MenuAction::Inert`] row.
fn is_question(item: &MenuItem) -> bool {
    item.checked && item.action == MenuAction::Inert
}

/// A row's height: one line, or as many lines as a confirm question wraps to
/// (at most [`QUESTION_MAX_LINES`]). Rendering and hit-testing both use it.
pub fn row_height(item: &MenuItem) -> f32 {
    if !is_question(item) {
        return ROW_HEIGHT;
    }
    let lines = item
        .label
        .chars()
        .count()
        .div_ceil(QUESTION_LINE_CHARS)
        .clamp(1, QUESTION_MAX_LINES);
    ROW_HEIGHT * lines as f32
}

pub fn menu_height(items: &[MenuItem]) -> f32 {
    items.iter().map(row_height).sum()
}

/// Whether these items are a confirm step ([`confirm_items`]).
pub fn is_confirm_step(items: &[MenuItem]) -> bool {
    items.iter().any(is_question)
}

/// How long a confirm step ignores presses on its rows after opening: the
/// second click of a double-click that chose the confirming entry would
/// otherwise land on the action row the step puts under the pointer.
pub const CONFIRM_ARM_DELAY: std::time::Duration = std::time::Duration::from_millis(500);

/// The input rules that keep a confirm step from accepting by accident: hover
/// never selects a row (so a stray Enter or Space accepts nothing), only an
/// arrow-key selection can be accepted from the keyboard, and a press counts
/// only once [`CONFIRM_ARM_DELAY`] has passed since the step opened. An
/// ordinary menu keeps its hover selection and immediate presses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MenuInputGuard {
    pub confirm: bool,
    pub opened_at: std::time::Duration,
}

impl MenuInputGuard {
    pub fn new(items: &[MenuItem], opened_at: std::time::Duration) -> Self {
        Self {
            confirm: is_confirm_step(items),
            opened_at,
        }
    }

    /// Hover may move the selection.
    pub fn hover_selects(&self) -> bool {
        !self.confirm
    }

    /// A press at `now` may start choosing a row.
    pub fn press_counts(&self, now: std::time::Duration) -> bool {
        !self.confirm || now.saturating_sub(self.opened_at) >= CONFIRM_ARM_DELAY
    }

    /// Enter/Space may accept the current selection, which came from the
    /// keyboard (`by_keyboard`) or from hover.
    pub fn key_accepts(&self, by_keyboard: bool) -> bool {
        !self.confirm || by_keyboard
    }
}

/// Position rows inward from their owning corner. Label padding keeps text
/// away from the compositor-owned hotspot.
pub fn menu_origin(corner: Corner, size: Vec2, items: &[MenuItem]) -> Vec2 {
    let right = matches!(corner, Corner::TopRight | Corner::BottomRight);
    let bottom = matches!(corner, Corner::BottomLeft | Corner::BottomRight);
    Vec2::new(
        if right {
            (size.x - MENU_WIDTH).max(0.0)
        } else {
            0.0
        },
        if bottom {
            (size.y - menu_height(items)).max(0.0)
        } else {
            0.0
        },
    )
}

pub fn hit_row(position: Vec2, origin: Vec2, items: &[MenuItem]) -> Option<usize> {
    let p = position - origin;
    if p.x < 0.0 || p.x >= MENU_WIDTH || p.y < 0.0 {
        return None;
    }
    let mut top = 0.0;
    for (index, item) in items.iter().enumerate() {
        let bottom = top + row_height(item);
        if p.y < bottom {
            return Some(index);
        }
        top = bottom;
    }
    None
}

/// The transparent root catches click-away on this output. The native host
/// owns the input and keyboard lifecycle; rows use the panel chrome palette.
pub fn spawn_menu(
    world: &mut World,
    mount: Entity,
    request: &CornerMenuRequest,
    size: Vec2,
) -> Vec<Entity> {
    let origin = menu_origin(request.corner, size, &request.items);
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
                        height: px(row_height(item)),
                        min_height: px(row_height(item)),
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
            // Only a mode row's disabled state means "current"; a confirm
            // step's question is disabled without a checkmark.
            let current = item.checked && matches!(item.action, MenuAction::Mode(_));
            let label = world
                .spawn((
                    Text::new(format!(
                        "{}{}",
                        if current { "✓  " } else { "    " },
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
            if is_question(item) {
                // Wraps inside the row, which row_height made tall enough.
                world.entity_mut(label).insert(Node {
                    width: percent(100),
                    ..default()
                });
            }
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
                confirm: None,
            };
            let items = menu_items(mode, std::slice::from_ref(&extra));
            assert_eq!(
                items.iter().map(|i| i.label.as_str()).collect::<Vec<_>>(),
                ["Pin", "Dock", "Hide", EDIT_PANELS_LABEL, "Tools"]
            );
            assert_eq!(items.iter().filter(|i| i.checked).count(), 1);
            let current = items.iter().find(|i| i.checked).unwrap();
            assert_eq!(current.action, MenuAction::Mode(mode));
            assert_eq!(current.command(Edge::Left), None);
            assert_eq!(items[3].action, MenuAction::EditPanels);
            assert_eq!(items[4].action, MenuAction::Extra(extra));
        }
    }

    #[test]
    fn edit_panels_is_built_in_on_every_corner_and_never_a_shell_command() {
        for mode in [PanelMode::Hidden, PanelMode::Pinned, PanelMode::Docked] {
            let items = menu_items(mode, &[]);
            assert_eq!(items.len(), 4, "present without config extras");
            let edit = &items[3];
            assert_eq!(
                (edit.label.as_str(), edit.checked, &edit.action),
                (EDIT_PANELS_LABEL, false, &MenuAction::EditPanels)
            );
            // Every corner summons an edge; the item is a Bus call, not a
            // mode change, on each of them.
            for corner in Corner::ALL {
                assert_eq!(edit.command(corner.summoned_edge()), None);
            }
        }
    }
    #[test]
    fn confirm_step_is_question_action_cancel_and_only_the_action_acts() {
        let extra = MenuExtra {
            label: "Restart session".into(),
            target: "desktop-session".into(),
            verb: "desktop.session.restart".into(),
            args: vec![],
            confirm: None,
        };
        let items = confirm_items("Restart the session?", "Restart", MenuAction::Extra(extra.clone()));
        assert_eq!(
            items.iter().map(|i| (i.label.as_str(), i.checked)).collect::<Vec<_>>(),
            [("Restart the session?", true), ("Restart", false), (CANCEL_LABEL, false)]
        );
        assert_eq!(items[1].action, MenuAction::Extra(extra));
        for item in &items {
            assert_eq!(item.command(Edge::Left), None, "a confirm row is never a mode change");
        }
        assert_eq!((&items[0].action, &items[2].action), (&MenuAction::Inert, &MenuAction::Inert));
        // The question renders disabled but without a checkmark.
        let mut world = World::new();
        let mount = world.spawn(Node::default()).id();
        let request = CornerMenuRequest {
            output: OutputKey::new("test-output").unwrap(),
            corner: Corner::TopLeft,
            items,
        };
        let rows = spawn_menu(&mut world, mount, &request, Vec2::new(1000.0, 800.0));
        assert!(world.get::<bevy::ui::InteractionDisabled>(rows[0]).is_some());
        let label = world.get::<Children>(rows[0]).unwrap()[0];
        assert!(!world.get::<Text>(label).unwrap().0.starts_with('✓'));
    }

    /// Review 6: a long question gets as many row lines as it wraps to, and
    /// hit-testing uses those real heights (not a fixed 32 px per row).
    #[test]
    fn a_long_question_is_a_taller_row_and_hit_testing_follows_it() {
        let question = "Restart the session? Every window closes; agent sessions resume.";
        assert!(question.chars().count() <= QUESTION_MAX_CHARS);
        let items = confirm_items(question, "Restart", MenuAction::Inert);
        let tall = row_height(&items[0]);
        assert_eq!(tall, ROW_HEIGHT * 3.0);
        assert_eq!(row_height(&items[1]), ROW_HEIGHT);
        assert_eq!(menu_height(&items), tall + 2.0 * ROW_HEIGHT);
        let short = confirm_items("Sure?", "Yes", MenuAction::Inert);
        assert_eq!(row_height(&short[0]), ROW_HEIGHT);
        let origin = Vec2::ZERO;
        // Inside the tall question: still row 0, never the action row.
        assert_eq!(hit_row(Vec2::new(20.0, tall - 1.0), origin, &items), Some(0));
        assert_eq!(hit_row(Vec2::new(20.0, tall + 1.0), origin, &items), Some(1));
        assert_eq!(hit_row(Vec2::new(20.0, tall + ROW_HEIGHT + 1.0), origin, &items), Some(2));
        assert_eq!(hit_row(Vec2::new(20.0, menu_height(&items) + 1.0), origin, &items), None);
        // A bottom corner anchors the whole (taller) menu above the edge.
        let size = Vec2::new(1000.0, 800.0);
        assert_eq!(menu_origin(Corner::BottomLeft, size, &items).y, 800.0 - menu_height(&items));
        // The rendered rows use the same heights.
        let mut world = World::new();
        let mount = world.spawn(Node::default()).id();
        let request = CornerMenuRequest {
            output: OutputKey::new("test-output").unwrap(),
            corner: Corner::BottomLeft,
            items,
        };
        let rows = spawn_menu(&mut world, mount, &request, size);
        assert_eq!(world.get::<Node>(rows[0]).unwrap().height, px(tall));
        assert_eq!(world.get::<Node>(rows[1]).unwrap().height, px(ROW_HEIGHT));
    }

    /// Reviews 3 and 4: in a confirm step hover never selects, only an
    /// arrow-key selection accepts from the keyboard, and presses count only
    /// after the arm delay. An ordinary menu is unchanged.
    #[test]
    fn a_confirm_step_guards_hover_keys_and_early_presses() {
        use std::time::Duration;
        let opened = Duration::from_secs(10);
        let confirm = MenuInputGuard::new(&confirm_items("Sure?", "Yes", MenuAction::Inert), opened);
        assert!(confirm.confirm);
        assert!(!confirm.hover_selects());
        assert!(!confirm.key_accepts(false), "Enter accepted a hover selection");
        assert!(confirm.key_accepts(true));
        assert!(!confirm.press_counts(opened + Duration::from_millis(200)), "a double-click's second press counted");
        assert!(confirm.press_counts(opened + CONFIRM_ARM_DELAY));
        let plain = MenuInputGuard::new(&menu_items(PanelMode::Hidden, &[]), opened);
        assert!(!plain.confirm);
        assert!(plain.hover_selects() && plain.key_accepts(false) && plain.press_counts(opened));
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
        let origin = menu_origin(request.corner, Vec2::new(1000.0, 800.0), &request.items);
        assert_eq!(
            hit_row(origin + Vec2::new(20.0, ROW_HEIGHT + 1.0), origin, &request.items),
            Some(1)
        );
        assert_eq!(hit_row(origin - Vec2::ONE, origin, &request.items), None);
    }
}
