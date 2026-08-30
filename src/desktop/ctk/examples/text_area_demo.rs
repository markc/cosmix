//! Human-facing compose exercise for CTK's wrapped text area.

use accesskit::Role;
use bevy::a11y::AccessibilityNode;
use bevy::feathers::theme::{ThemeBackgroundColor, ThemeTextColor, UiTheme};
use bevy::feathers::{dark_theme::create_dark_theme, FeathersPlugins};
use bevy::input_focus::tab_navigation::TabIndex;
use bevy::picking::Pickable;
use bevy::prelude::*;
use bevy::ui_widgets::Activate;
use ctk::prelude::*;
use ctk::theme::tokens;

#[derive(Resource)]
struct DemoArea(Entity);

#[derive(Component)]
struct CharacterCount;

#[derive(Component, Clone, Copy)]
enum HistoryButton {
    Undo,
    Redo,
}

fn main() {
    App::new()
        .add_plugins(DefaultPlugins.set(WindowPlugin {
            primary_window: Some(Window {
                title: "CTK text area — compose demo".into(),
                resolution: (820, 620).into(),
                resizable: true,
                ..default()
            }),
            ..default()
        }))
        .add_plugins((
            FeathersPlugins,
            CtkThemePlugin::default(),
            CtkTextAreaPlugin,
        ))
        .add_systems(Startup, setup)
        .add_observer(on_history_activate)
        .add_observer(on_body_changed)
        .add_observer(on_body_submitted)
        .run();
}

fn setup(mut commands: Commands, mut theme: ResMut<UiTheme>, mut state: ResMut<ThemeState>) {
    const BODY: &str = "Hello,\n\nResize this window to exercise soft wrapping. The body keeps \
        the caret visible while it scrolls and retains the goal column through short visual \
        lines.\n\nRegards,\nCosmix";

    *theme = UiTheme(create_dark_theme());
    apply_theme(&mut theme, &mut state, &ThemeSpec::builtin());
    commands.spawn(Camera2d);

    let to = spawn_text_field(
        &mut commands,
        CtkTextFieldProps::new("mail@example.com", "To").max_length(512),
    );
    let subject = spawn_text_field(
        &mut commands,
        CtkTextFieldProps::new("Wrapped text-area smoke test", "Subject").max_length(998),
    );
    let body = spawn_text_area(
        &mut commands,
        CtkTextAreaProps::new(BODY, "Message body")
            .max_len(20_000)
            .visible_lines(14)
            .history_limit(128)
            .min_height(240.0),
    );

    let count = commands
        .spawn((
            Text::new(format!("{} / 20000 characters", BODY.chars().count())),
            TextFont::from_font_size(12.0),
            ThemeTextColor(tokens::TEXT_DIM),
            CharacterCount,
        ))
        .id();
    let undo = demo_button(&mut commands, "Undo", HistoryButton::Undo);
    let redo = demo_button(&mut commands, "Redo", HistoryButton::Redo);

    let to_row = labelled_field_row(&mut commands, "To", to.root);
    let subject_row = labelled_field_row(&mut commands, "Subject", subject.root);
    let controls = commands
        .spawn(Node {
            width: percent(100),
            height: px(34),
            flex_direction: FlexDirection::Row,
            align_items: AlignItems::Center,
            column_gap: px(8),
            ..default()
        })
        .add_children(&[undo, redo, count])
        .id();
    let compose_heading = heading(&mut commands, "Compose");
    let keyboard_hint = hint(
        &mut commands,
        "Tab: To → Subject → Body → Undo → Redo  •  Ctrl+Z / Ctrl+Shift+Z  •  \
         Ctrl+Enter submits  •  Enter inserts a newline",
    );

    commands
        .spawn((
            Node {
                width: percent(100),
                height: percent(100),
                min_width: px(360),
                flex_direction: FlexDirection::Column,
                padding: UiRect::all(px(24)),
                row_gap: px(10),
                ..default()
            },
            ThemeBackgroundColor(tokens::PANEL),
        ))
        .add_children(&[
            compose_heading,
            to_row,
            subject_row,
            body.root,
            controls,
            keyboard_hint,
        ]);

    commands.insert_resource(DemoArea(body.input));
}

fn labelled_field_row(commands: &mut Commands, label: &str, field: Entity) -> Entity {
    let label = commands
        .spawn((
            Node {
                width: px(72),
                min_width: px(72),
                ..default()
            },
            Text::new(label),
            TextFont::from_font_size(13.0),
            ThemeTextColor(tokens::TEXT_DIM),
        ))
        .id();
    commands
        .spawn(Node {
            width: percent(100),
            height: px(38),
            min_height: px(38),
            flex_direction: FlexDirection::Row,
            align_items: AlignItems::Center,
            column_gap: px(8),
            ..default()
        })
        .add_children(&[label, field])
        .id()
}

fn heading(commands: &mut Commands, value: &str) -> Entity {
    commands
        .spawn((
            Text::new(value),
            TextFont::from_font_size(22.0),
            ThemeTextColor(tokens::TEXT),
        ))
        .id()
}

fn hint(commands: &mut Commands, value: &str) -> Entity {
    commands
        .spawn((
            Text::new(value),
            TextFont::from_font_size(12.0),
            ThemeTextColor(tokens::TEXT_DIM),
        ))
        .id()
}

fn demo_button(commands: &mut Commands, label: &str, action: HistoryButton) -> Entity {
    let text = commands
        .spawn((
            Text::new(label),
            TextFont::from_font_size(12.0),
            ThemeTextColor(tokens::TEXT),
            Pickable::IGNORE,
        ))
        .id();
    let mut accessibility = accesskit::Node::new(Role::Button);
    accessibility.set_label(label);
    commands
        .spawn((
            Button,
            Pickable::default(),
            TabIndex(0),
            AccessibilityNode::from(accessibility),
            action,
            Node {
                height: px(30),
                min_width: px(64),
                padding: UiRect::horizontal(px(9)),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                border_radius: BorderRadius::all(px(4)),
                ..default()
            },
            ThemeBackgroundColor(tokens::CONTROL),
        ))
        .add_child(text)
        .id()
}

fn on_history_activate(
    activate: On<Activate>,
    buttons: Query<&HistoryButton>,
    area: Res<DemoArea>,
    mut commands: Commands,
) {
    let Ok(action) = buttons.get(activate.entity) else {
        return;
    };
    match action {
        HistoryButton::Undo => undo_text_area(&mut commands, area.0),
        HistoryButton::Redo => redo_text_area(&mut commands, area.0),
    }
}

fn on_body_changed(
    event: On<CtkTextAreaChanged>,
    area: Res<DemoArea>,
    mut count: Query<&mut Text, With<CharacterCount>>,
) {
    if event.area != area.0 {
        return;
    }
    if let Ok(mut count) = count.single_mut() {
        count.0 = format!("{} / 20000 characters", event.value.chars().count());
    }
}

fn on_body_submitted(event: On<CtkTextAreaSubmitted>) {
    info!(
        characters = event.value.chars().count(),
        "compose body submitted with Ctrl+Enter"
    );
}
