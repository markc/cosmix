//! Live smoke test for CTK drag-and-drop.

use bevy::picking::events::{Click, Pointer};
use bevy::picking::Pickable;
use bevy::prelude::*;
use ctk::prelude::*;

#[derive(Component)]
struct DemoStatus;

#[derive(Component)]
struct DemoTarget;

fn main() {
    App::new()
        .add_plugins(DefaultPlugins.set(WindowPlugin {
            primary_window: Some(Window {
                title: "CTK drag-and-drop demo".into(),
                resolution: (860, 520).into(),
                ..default()
            }),
            ..default()
        }))
        .add_plugins(DndPlugin)
        .add_systems(Startup, setup)
        .add_systems(Update, resolve_drop.in_set(AppResolve))
        .add_systems(Update, (paint_highlight, report_drop, report_cancel))
        .add_observer(on_source_click)
        .run();
}

fn setup(mut commands: Commands) {
    commands.spawn(Camera2d);
    let status = commands
        .spawn((
            Text::new("Drag the card onto either target. Escape or leave the window to cancel."),
            TextFont::from_font_size(15.0),
            TextColor(Color::srgb(0.75, 0.79, 0.86)),
            Pickable::IGNORE,
            DemoStatus,
        ))
        .id();

    let ghost = GhostBuilder::new(|root, commands| {
        commands
            .entity(root)
            .insert(BackgroundColor(Color::srgba(0.10, 0.14, 0.22, 0.94)));
        let icon = commands
            .spawn((
                Text::new("↗"),
                TextFont::from_font_size(20.0),
                TextColor(Color::srgb(0.35, 0.78, 1.0)),
                Pickable::IGNORE,
            ))
            .id();
        let label = commands
            .spawn((
                Text::new("demo-item.txt"),
                TextFont::from_font_size(15.0),
                TextColor(Color::WHITE),
                Pickable::IGNORE,
            ))
            .id();
        commands.entity(root).add_children(&[icon, label]);
    });

    let source = commands
        .spawn((
            Node {
                width: px(220),
                height: px(68),
                padding: UiRect::all(px(14)),
                align_items: AlignItems::Center,
                border_radius: BorderRadius::all(px(7)),
                ..default()
            },
            BackgroundColor(Color::srgb(0.13, 0.20, 0.32)),
            DragSource::new(DragPayload::Paths(vec!["/tmp/demo-item.txt".into()]), ghost),
        ))
        .with_children(|source| {
            source.spawn((
                Text::new("⠿  demo-item.txt"),
                TextFont::from_font_size(18.0),
                TextColor(Color::WHITE),
                Pickable::IGNORE,
            ));
        })
        .id();

    let left = spawn_target(&mut commands, "Copy destination");
    let right = spawn_target(&mut commands, "Move destination");
    let target_row = commands
        .spawn((Node {
            width: percent(100),
            flex_direction: FlexDirection::Row,
            column_gap: px(18),
            ..default()
        },))
        .add_children(&[left, right])
        .id();

    commands
        .spawn((
            Node {
                width: percent(100),
                min_height: percent(100),
                flex_direction: FlexDirection::Column,
                padding: UiRect::all(px(28)),
                row_gap: px(28),
                ..default()
            },
            BackgroundColor(Color::srgb(0.045, 0.055, 0.075)),
        ))
        .add_children(&[status, source, target_row]);
}

fn spawn_target(commands: &mut Commands, label: &str) -> Entity {
    let text = commands
        .spawn((
            Text::new(label),
            TextFont::from_font_size(17.0),
            TextColor(Color::srgb(0.78, 0.82, 0.90)),
            Pickable::IGNORE,
        ))
        .id();
    commands
        .spawn((
            Node {
                width: px(330),
                height: px(180),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                border: UiRect::all(px(2)),
                border_radius: BorderRadius::all(px(8)),
                ..default()
            },
            BackgroundColor(Color::srgb(0.08, 0.09, 0.12)),
            BorderColor::all(Color::srgb(0.25, 0.29, 0.37)),
            DropTarget,
            DemoTarget,
        ))
        .add_child(text)
        .id()
}

fn resolve_drop(
    mut proposals: MessageReader<AcceptanceProposal>,
    mut acceptances: MessageWriter<DropAcceptance>,
) {
    for proposal in proposals.read() {
        acceptances.write(DropAcceptance {
            proposal_id: proposal.proposal_id,
            revision: proposal.revision,
            allowed_actions: ActionMask::ALL,
            preferred: DropAction::Copy,
        });
    }
}

fn paint_highlight(
    mut transitions: MessageReader<DndHighlightChanged>,
    mut targets: Query<(&mut BackgroundColor, &mut BorderColor), With<DemoTarget>>,
) {
    for transition in transitions.read() {
        let Ok((mut background, mut border)) = targets.get_mut(transition.target) else {
            continue;
        };
        if transition.highlighted {
            background.0 = Color::srgb(0.08, 0.22, 0.30);
            *border = BorderColor::all(Color::srgb(0.25, 0.82, 1.0));
        } else {
            background.0 = Color::srgb(0.08, 0.09, 0.12);
            *border = BorderColor::all(Color::srgb(0.25, 0.29, 0.37));
        }
    }
}

fn report_drop(
    mut drops: MessageReader<DndDrop>,
    mut complete: MessageWriter<DropComplete>,
    mut status: Single<&mut Text, With<DemoStatus>>,
) {
    for drop in drops.read() {
        status.0 = format!(
            "Delivered {:?} to {:?} as {:?}",
            drop.payload, drop.target, drop.action
        );
        complete.write(DropComplete {
            delivery_id: drop.delivery_id,
            outcome: DropOutcome::Completed(drop.action),
        });
    }
}

fn report_cancel(
    mut cancellations: MessageReader<DndCancelled>,
    mut status: Single<&mut Text, With<DemoStatus>>,
) {
    for cancellation in cancellations.read() {
        status.0 = format!("Cancelled: {:?}", cancellation.reason);
    }
}

fn on_source_click(click: On<Pointer<Click>>, session: Res<DragSession>) {
    if dnd_click_is_blocked(click.entity, &session) {
        return;
    }
    info!(source = ?click.entity, "ordinary source click");
}
