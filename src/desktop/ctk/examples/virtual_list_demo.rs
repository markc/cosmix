//! Human-facing 100,000-row exercise for CTK's virtual list.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use bevy::a11y::AccessibilityNode;
use bevy::ecs::system::SystemParam;
use bevy::feathers::theme::{ThemeBackgroundColor, ThemeTextColor, UiTheme};
use bevy::feathers::{dark_theme::create_dark_theme, FeathersPlugins};
use bevy::picking::events::{Click, Pointer};
use bevy::picking::Pickable;
use bevy::prelude::*;
use bevy::text::{EditableText, FontWeight};
use ctk::prelude::*;
use ctk::theme::tokens;

#[derive(Clone)]
struct MailModel {
    ids: Arc<RwLock<Vec<RowId>>>,
}

impl VirtualListModel for MailModel {
    fn len(&self) -> usize {
        self.ids.read().expect("demo model lock poisoned").len()
    }

    fn row_id(&self, index: usize) -> RowId {
        self.ids.read().expect("demo model lock poisoned")[index]
    }

    fn bind(&self, world: &mut World, content: Entity, _index: usize) {
        let row = world
            .get::<ChildOf>(content)
            .expect("CTK bind content has a row parent")
            .parent();
        let metadata = *world
            .get::<VirtualListRow>(row)
            .expect("CTK binds after inserting row metadata");
        let primary = if metadata.selected {
            tokens::ROW_SELECTED_TEXT
        } else {
            tokens::TEXT
        };
        let unread = metadata.row_id.0.is_multiple_of(7);
        let sender = format!("Sender {:05}", metadata.row_id.0 % 10_000);
        let subject = format!(
            "{}Synthetic conversation {} — recycled entity row",
            if unread { "New: " } else { "" },
            metadata.row_id.0
        );
        let date = format!(
            "{:02}:{:02}",
            (metadata.row_id.0 / 60) % 24,
            metadata.row_id.0 % 60
        );
        if let Some(mut node) = world.get_mut::<Node>(content) {
            node.padding = UiRect::horizontal(px(8));
            node.column_gap = px(10);
        }
        let weight = if unread {
            FontWeight::BOLD
        } else {
            FontWeight::NORMAL
        };
        let sender_cell = mail_cell(
            world,
            sender.clone(),
            Node {
                width: px(160),
                min_width: px(160),
                ..default()
            },
            primary.clone(),
            weight,
        );
        let subject_cell = mail_cell(
            world,
            subject,
            Node {
                min_width: px(0),
                flex_grow: 1.0,
                overflow: Overflow::clip(),
                ..default()
            },
            primary,
            weight,
        );
        let date_cell = mail_cell(
            world,
            date.clone(),
            Node {
                width: px(52),
                min_width: px(52),
                justify_content: JustifyContent::FlexEnd,
                ..default()
            },
            tokens::TEXT_DIM,
            weight,
        );
        world
            .entity_mut(content)
            .add_children(&[sender_cell, subject_cell, date_cell]);

        let mut accessible = world
            .get_mut::<AccessibilityNode>(content)
            .expect("CTK bind content has accessibility metadata");
        accessible.set_label(format!(
            "{sender}, conversation {}, {date}",
            metadata.row_id.0,
        ));
    }
}

fn mail_cell(
    world: &mut World,
    text: String,
    node: Node,
    colour: bevy::feathers::theme::ThemeToken,
    weight: FontWeight,
) -> Entity {
    world
        .spawn((
            node,
            Text::new(text),
            TextLayout::no_wrap(),
            TextFont::from_font_size(13.0).with_font_weight(weight),
            ThemeTextColor(colour),
            Pickable::IGNORE,
        ))
        .id()
}

#[derive(Resource)]
struct Demo {
    list: Entity,
    jump_input: Entity,
    ids: Arc<RwLock<Vec<RowId>>>,
    stress: bool,
    stress_accumulator: Duration,
    next_id: u64,
    report_accumulator: Duration,
    benchmark_remaining: u16,
    benchmark_index: usize,
}

#[derive(Component)]
struct JumpButton;

#[derive(Component)]
struct StressButton;

#[derive(Component, Clone, Copy)]
struct ModeButton(SelectionMode);

#[derive(SystemParam)]
struct ControlParams<'w, 's> {
    jump_buttons: Query<'w, 's, (), With<JumpButton>>,
    stress_buttons: Query<'w, 's, (), With<StressButton>>,
    mode_buttons: Query<'w, 's, &'static ModeButton>,
    inputs: Query<'w, 's, &'static EditableText>,
}

fn main() {
    App::new()
        .add_plugins(DefaultPlugins.set(WindowPlugin {
            primary_window: Some(Window {
                title: "CTK virtual list — 100,000 rows".into(),
                resolution: (1100, 760).into(),
                ..default()
            }),
            ..default()
        }))
        .add_plugins((
            FeathersPlugins,
            CtkThemePlugin::default(),
            CtkTextFieldPlugin,
            VirtualListPlugin,
        ))
        .add_systems(Startup, setup)
        .add_systems(
            Update,
            (exercise_steady_state_scroll, stress_model, print_latency),
        )
        .add_observer(on_control_click)
        .add_observer(report_selection)
        .add_observer(report_activation)
        .run();
}

fn setup(mut commands: Commands, mut theme: ResMut<UiTheme>, mut theme_state: ResMut<ThemeState>) {
    *theme = UiTheme(create_dark_theme());
    apply_theme(&mut theme, &mut theme_state, &ThemeSpec::builtin());
    commands.spawn(Camera2d);

    let ids = Arc::new(RwLock::new((0..100_000).map(RowId).collect()));
    let model = MailModel { ids: ids.clone() };
    let jump = spawn_text_field(
        &mut commands,
        CtkTextFieldProps::new("50000", "Jump to row index")
            .max_length(6)
            .select_all(true),
    );
    commands.entity(jump.root).insert(Node {
        width: px(130),
        flex_grow: 0.0,
        ..default()
    });

    let jump_button = demo_button(&mut commands, "Jump");
    commands.entity(jump_button).insert(JumpButton);
    let single = demo_button(&mut commands, "Single");
    commands
        .entity(single)
        .insert(ModeButton(SelectionMode::Single));
    let range = demo_button(&mut commands, "Shift range");
    commands
        .entity(range)
        .insert(ModeButton(SelectionMode::Contiguous));
    let disjoint = demo_button(&mut commands, "Ctrl disjoint");
    commands
        .entity(disjoint)
        .insert(ModeButton(SelectionMode::Disjoint));
    let stress = demo_button(&mut commands, "Stress 100 rows/s");
    commands.entity(stress).insert(StressButton);

    let list = spawn_virtual_list(
        &mut commands,
        VirtualListProps::new(30.0, 620.0, "Synthetic mail conversations")
            .selection_mode(SelectionMode::Disjoint),
        model,
    );
    commands.entity(list.root).insert((
        ThemeBackgroundColor(tokens::SURFACE),
        BorderColor::all(Color::NONE),
        bevy::feathers::theme::ThemeBorderColor(tokens::BORDER),
    ));

    let controls = commands
        .spawn(Node {
            width: percent(100),
            height: px(38),
            flex_direction: FlexDirection::Row,
            align_items: AlignItems::Center,
            column_gap: px(8),
            ..default()
        })
        .add_children(&[jump.root, jump_button, single, range, disjoint, stress])
        .id();
    let help = commands
        .spawn((
            Text::new(
                "Wheel/trackpad, scrollbar, arrows, PageUp/Down, Home/End; double-click or Enter activates.",
            ),
            TextFont::from_font_size(12.0),
            ThemeTextColor(tokens::TEXT_DIM),
        ))
        .id();
    commands
        .spawn((
            Node {
                width: percent(100),
                height: percent(100),
                padding: UiRect::all(px(18)),
                flex_direction: FlexDirection::Column,
                row_gap: px(8),
                ..default()
            },
            ThemeBackgroundColor(tokens::PANEL),
        ))
        .add_children(&[controls, help, list.root]);

    commands.insert_resource(Demo {
        list: list.root,
        jump_input: jump.input,
        ids,
        stress: false,
        stress_accumulator: Duration::ZERO,
        next_id: 100_000,
        report_accumulator: Duration::ZERO,
        benchmark_remaining: 120,
        benchmark_index: 0,
    });
}

fn demo_button(commands: &mut Commands, label: &str) -> Entity {
    let text = commands
        .spawn((
            Text::new(label),
            TextFont::from_font_size(12.0),
            ThemeTextColor(tokens::TEXT),
            Pickable::IGNORE,
        ))
        .id();
    commands
        .spawn((
            Button,
            Pickable::default(),
            Node {
                height: px(30),
                min_width: px(72),
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

fn on_control_click(
    click: On<Pointer<Click>>,
    controls: ControlParams,
    mut demo: ResMut<Demo>,
    mut lists: Query<&mut VirtualList>,
    mut commands: Commands,
) {
    if controls.jump_buttons.contains(click.entity) {
        let Some(index) = controls
            .inputs
            .get(demo.jump_input)
            .ok()
            .and_then(|input| input.value().to_string().parse::<usize>().ok())
        else {
            return;
        };
        virtual_list_scroll_to(&mut commands, demo.list, index, Align::Center);
    } else if controls.stress_buttons.contains(click.entity) {
        demo.stress = !demo.stress;
        println!(
            "insert/remove stress {}",
            if demo.stress { "enabled" } else { "disabled" }
        );
    } else if let Ok(mode) = controls.mode_buttons.get(click.entity) {
        if let Ok(mut list) = lists.get_mut(demo.list) {
            list.set_selection_mode(mode.0);
            println!("selection mode: {:?}", mode.0);
        }
    }
}

fn stress_model(time: Res<Time>, mut demo: ResMut<Demo>, mut commands: Commands) {
    if !demo.stress {
        return;
    }
    demo.stress_accumulator += time.delta();
    let step = Duration::from_millis(10);
    let mut mutations = 0;
    while demo.stress_accumulator >= step && mutations < 20 {
        demo.stress_accumulator -= step;
        let next_id = demo.next_id;
        demo.next_id += 1;
        {
            let mut ids = demo.ids.write().expect("demo model lock poisoned");
            ids.remove(100);
            ids.insert(100, RowId(next_id));
        }
        virtual_list_changed(&mut commands, demo.list, ChangeHint::Removed(100..101));
        virtual_list_changed(&mut commands, demo.list, ChangeHint::Inserted(100..101));
        mutations += 1;
    }
}

fn exercise_steady_state_scroll(mut demo: ResMut<Demo>, mut commands: Commands) {
    if demo.benchmark_remaining == 0 {
        return;
    }
    demo.benchmark_remaining -= 1;
    demo.benchmark_index += 1;
    virtual_list_scroll_to(&mut commands, demo.list, demo.benchmark_index, Align::Start);
}

fn print_latency(time: Res<Time>, mut demo: ResMut<Demo>, lists: Query<&VirtualList>) {
    demo.report_accumulator += time.delta();
    if demo.report_accumulator < Duration::from_secs(5) {
        return;
    }
    demo.report_accumulator = Duration::ZERO;
    if let Ok(list) = lists.get(demo.list) {
        let latency = list.latency();
        let p99_us = latency.quantile_us(0.99);
        let target = if latency.count() > 0 && p99_us <= 2_000 {
            "PASS"
        } else {
            "NOT YET"
        };
        println!(
            "virtual-list full CTK frame {} (p99≤2.0ms target: {target}), realised={} model={}",
            latency.summary(),
            list.realised_range().len(),
            list.model_len()
        );
    }
}

fn report_selection(event: On<VirtualListSelectionChanged>) {
    println!("selected {} stable row ids", event.selected.len());
}

fn report_activation(event: On<VirtualListRowActivated>) {
    println!(
        "activated {:?} at model index {}",
        event.row_id, event.index
    );
}
