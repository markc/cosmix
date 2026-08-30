//! Shared modal-dialog chrome used by CTK presenters.

use accesskit::Role;
use bevy::a11y::AccessibilityNode;
use bevy::feathers::theme::{ThemeBackgroundColor, ThemeBorderColor, ThemeTextColor, ThemeToken};
use bevy::input_focus::tab_navigation::TabGroup;
use bevy::prelude::*;
use bevy::ui::{FocusPolicy, GlobalZIndex};

use crate::theme::tokens;

/// Marker for the full-screen modal backdrop.
#[derive(Component)]
pub struct DialogShellRoot;

/// Marker for the visible dialog panel.
#[derive(Component)]
pub struct DialogShellPanel;

/// Reusable sizing and semantic properties for modal dialog chrome.
pub struct DialogShell {
    pub title: String,
    pub role: Role,
    pub accent: ThemeToken,
    pub z_index: i32,
    pub width: Val,
    pub min_width: Val,
    pub max_width: Val,
    pub height: Val,
    pub min_height: Val,
}

impl DialogShell {
    pub fn new(title: impl Into<String>, role: Role, accent: ThemeToken, z_index: i32) -> Self {
        Self {
            title: title.into(),
            role,
            accent,
            z_index,
            width: percent(44),
            min_width: px(360),
            max_width: px(620),
            height: Val::Auto,
            min_height: Val::Auto,
        }
    }

    pub fn size(mut self, width: Val, min_width: Val, max_width: Val) -> Self {
        self.width = width;
        self.min_width = min_width;
        self.max_width = max_width;
        self
    }

    pub fn height(mut self, height: Val, min_height: Val) -> Self {
        self.height = height;
        self.min_height = min_height;
        self
    }
}

/// Entities exposed so a presenter can populate the body and action row.
#[derive(Clone, Copy, Debug)]
pub struct DialogShellEntities {
    pub root: Entity,
    pub panel: Entity,
    pub body: Entity,
    pub actions: Entity,
}

/// Spawn a scrim, accessible panel, body and action row.
pub fn spawn_dialog_shell(commands: &mut Commands, shell: DialogShell) -> DialogShellEntities {
    let title = commands
        .spawn((
            Text::new(&shell.title),
            TextFont::from_font_size(17.0),
            ThemeTextColor(tokens::TEXT),
        ))
        .id();
    let body = commands
        .spawn(Node {
            width: percent(100),
            flex_grow: 1.0,
            min_height: px(0),
            flex_direction: FlexDirection::Column,
            row_gap: px(10),
            ..default()
        })
        .id();
    let actions = commands
        .spawn(Node {
            width: percent(100),
            flex_direction: FlexDirection::Row,
            justify_content: JustifyContent::End,
            column_gap: px(7),
            ..default()
        })
        .id();

    let mut accessible = accesskit::Node::new(shell.role);
    accessible.set_label(&*shell.title);
    accessible.set_modal();
    let panel = commands
        .spawn((
            Node {
                width: shell.width,
                min_width: shell.min_width,
                max_width: shell.max_width,
                height: shell.height,
                min_height: shell.min_height,
                flex_direction: FlexDirection::Column,
                row_gap: px(12),
                padding: UiRect::all(px(16)),
                border: UiRect::all(px(1)),
                border_radius: BorderRadius::all(px(7)),
                ..default()
            },
            ThemeBackgroundColor(tokens::PANEL),
            BorderColor::all(Color::NONE),
            ThemeBorderColor(shell.accent),
            DialogShellPanel,
            AccessibilityNode::from(accessible),
        ))
        .add_children(&[title, body, actions])
        .id();
    let root = commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                left: px(0),
                right: px(0),
                top: px(0),
                bottom: px(0),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                ..default()
            },
            ThemeBackgroundColor(tokens::SCRIM),
            GlobalZIndex(shell.z_index),
            FocusPolicy::Block,
            TabGroup::modal(),
            DialogShellRoot,
        ))
        .add_child(panel)
        .id();

    DialogShellEntities {
        root,
        panel,
        body,
        actions,
    }
}
