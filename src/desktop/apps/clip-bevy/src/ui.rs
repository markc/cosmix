use crate::bus::{Bus, Event, Request, Snapshot};
use bevy::{
    feathers::{dark_theme::create_dark_theme, theme::UiTheme},
    input_focus::InputFocus,
    prelude::*,
    text::EditableText,
    ui_widgets::{Activate, Button as WidgetButton, ScrollArea},
    window::PrimaryWindow,
    winit::{EventLoopProxyWrapper, WinitUserEvent},
};
use ctk::prelude::*;
use serde_json::Value;
use std::{
    sync::Arc,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Resource, Default)]
pub struct Panel {
    snapshot: Snapshot,
    filter: String,
    dirty: bool,
    clear: Option<Instant>,
    clear_token: u64,
    view: Option<View>,
}

struct View {
    root: Entity,
    camera: Entity,
    search: Entity,
    status: Entity,
    table: Entity,
    remote: Entity,
    pause_label: Entity,
    clear_label: Entity,
    clear_button: Entity,
}

#[derive(Component, Clone)]
pub enum Action {
    Pick(Value),
    RemotePick(Value),
    Pause,
    Clear,
}

pub fn colour(rgb: u32) -> Color {
    Color::srgb_u8((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

pub fn window() -> Window {
    Window {
        title: "CosMix Clipboard".into(),
        name: Some("dev.cosmix.clip-bevy".into()),
        resolution: (720, 520).into(),
        ..default()
    }
}

pub fn start_bus(mut commands: Commands, proxy: Res<EventLoopProxyWrapper>) {
    let proxy = (**proxy).clone();
    commands.insert_resource(Bus::start(Arc::new(move || {
        let _ = proxy.send_event(WinitUserEvent::WakeUp);
    })));
}

pub fn setup(
    mut commands: Commands,
    mut theme: ResMut<UiTheme>,
    mut theme_state: ResMut<ThemeState>,
    mut panel: ResMut<Panel>,
) {
    *theme = UiTheme(create_dark_theme());
    apply_theme(&mut theme, &mut theme_state, &ThemeSpec::builtin());
    panel.view = Some(create_view(&mut commands, &panel.filter));
    panel.dirty = true;
}

fn label(commands: &mut Commands, text: &str, rgb: u32, size: f32) -> Entity {
    commands
        .spawn((
            Text::new(text),
            TextFont::from_font_size(size),
            TextColor(colour(rgb)),
            TextLayout::no_wrap(),
        ))
        .id()
}

fn button(commands: &mut Commands, title: &str, action: Action) -> (Entity, Entity) {
    let text = label(commands, title, 0xcfd3da, 12.0);
    let root = commands
        .spawn((
            WidgetButton,
            Interaction::default(),
            action,
            Node {
                padding: UiRect::axes(px(9), px(5)),
                flex_shrink: 0.0,
                align_items: AlignItems::Center,
                ..default()
            },
            BackgroundColor(colour(0x20242d)),
        ))
        .add_child(text)
        .id();
    (root, text)
}

fn column(commands: &mut Commands) -> Entity {
    commands
        .spawn(Node {
            width: percent(100),
            min_height: px(0),
            flex_direction: FlexDirection::Column,
            row_gap: px(3),
            ..default()
        })
        .id()
}

fn create_view(commands: &mut Commands, filter: &str) -> View {
    let camera = commands.spawn(Camera2d).id();
    let root = commands
        .spawn((
            Node {
                width: percent(100),
                height: percent(100),
                padding: UiRect::all(px(14)),
                flex_direction: FlexDirection::Column,
                row_gap: px(8),
                ..default()
            },
            BackgroundColor(colour(0x1b1d23)),
        ))
        .id();
    let heading = label(commands, "CosMix Clipboard", 0xcfd3da, 18.0);
    let status = label(commands, "Connecting…", 0xd9a05b, 12.0);
    let titles = column(commands);
    commands.entity(titles).entry::<Node>().and_modify(|mut n| {
        n.width = auto();
        n.flex_grow = 1.0;
    });
    commands.entity(titles).add_children(&[heading, status]);
    let search = spawn_text_field(commands, CtkTextFieldProps::new(filter, "Search…"));
    commands
        .entity(search.root)
        .entry::<Node>()
        .and_modify(|mut n| {
            n.width = px(170);
            n.flex_grow = 0.0;
            n.flex_shrink = 0.0;
        });
    let (pause, pause_label) = button(commands, "Pause", Action::Pause);
    let (clear_button, clear_label) = button(commands, "Clear", Action::Clear);
    let header = commands
        .spawn(Node {
            width: percent(100),
            column_gap: px(8),
            align_items: AlignItems::Center,
            flex_shrink: 0.0,
            ..default()
        })
        .add_children(&[titles, search.root, pause, clear_button])
        .id();
    let columns = commands
        .spawn(Node {
            width: percent(100),
            column_gap: px(8),
            padding: UiRect::horizontal(px(10)),
            ..default()
        })
        .id();
    for (text, width) in [
        ("ID", 32.0),
        ("BYTES", 50.0),
        ("AGE", 34.0),
        ("PREVIEW", 350.0),
    ] {
        let cell = label(commands, text, 0x6b7280, 11.0);
        commands.entity(cell).insert(Node {
            width: px(width),
            flex_shrink: 0.0,
            ..default()
        });
        commands.entity(columns).add_child(cell);
    }
    let table = column(commands);
    let scroll = commands
        .spawn((
            Node {
                flex_grow: 1.0,
                min_height: px(0),
                width: percent(100),
                overflow: Overflow::scroll_y(),
                ..default()
            },
            ScrollArea,
            ScrollPosition::default(),
        ))
        .add_child(table)
        .id();
    let remote = column(commands);
    commands
        .entity(remote)
        .entry::<Node>()
        .and_modify(|mut n| n.flex_shrink = 0.0);
    let footer = label(commands, "Click an entry to make it the live selection, then paste (Ctrl+V). Updates arrive live over the desktop.clipboard.changed Bus topic.", 0x6b7280, 11.0);
    commands.entity(footer).insert(TextLayout::default());
    commands
        .entity(root)
        .add_children(&[header, columns, scroll, remote, footer]);
    View {
        root,
        camera,
        search: search.input,
        status,
        table,
        remote,
        pause_label,
        clear_label,
        clear_button,
    }
}

pub fn receive(
    mut commands: Commands,
    bus: Res<Bus>,
    mut panel: ResMut<Panel>,
    windows: Query<Entity, With<Window>>,
) {
    for event in bus.events.try_iter() {
        match event {
            Event::Snapshot(snapshot) => {
                panel.snapshot = *snapshot;
                panel.dirty = true;
            }
            Event::Error(error) => {
                panel.snapshot.error = error;
                panel.dirty = true;
            }
            Event::ClearExpired(token) if token == panel.clear_token => {
                panel.clear = None;
                panel.dirty = true;
            }
            Event::ClearExpired(_) => {}
            Event::Toggle => {
                // The Window entity must be destroyed, not made invisible.
                if !windows.is_empty() {
                    for entity in &windows {
                        commands.entity(entity).despawn();
                    }
                    if let Some(view) = panel.view.take() {
                        commands.entity(view.root).despawn();
                        commands.entity(view.camera).despawn();
                    }
                } else {
                    if let Some(view) = panel.view.take() {
                        commands.entity(view.root).despawn();
                        commands.entity(view.camera).despawn();
                    }
                    commands.spawn((window(), PrimaryWindow));
                    panel.view = Some(create_view(&mut commands, &panel.filter));
                    panel.dirty = true;
                }
            }
        }
    }
}

pub fn search(
    inputs: Query<&EditableText, Changed<EditableText>>,
    keys: Res<ButtonInput<KeyCode>>,
    focus: Res<InputFocus>,
    bus: Res<Bus>,
    mut panel: ResMut<Panel>,
) {
    let Some(view) = &panel.view else {
        return;
    };
    let search = view.search;
    if let Ok(input) = inputs.get(search) {
        let text = input.value().to_string();
        if text != panel.filter {
            panel.filter = text;
            panel.dirty = true;
        }
    }
    if focus.get() == Some(search) && keys.just_pressed(KeyCode::Enter) {
        let query = panel.filter.clone();
        send(&bus, &mut panel, Request::Search(query));
    }
}

fn send(bus: &Bus, panel: &mut Panel, request: Request) {
    if let Err(e) = bus.requests.try_send(request) {
        panel.snapshot.error = format!("Action not queued: {e}");
        panel.dirty = true;
    }
}

pub fn activate(
    event: On<Activate>,
    actions: Query<&Action>,
    bus: Res<Bus>,
    mut panel: ResMut<Panel>,
) {
    let Ok(action) = actions.get(event.entity) else {
        return;
    };
    let request = match action {
        Action::Pick(id) => Request::Pick(id.clone()),
        Action::RemotePick(id) => Request::RemotePick(id.clone()),
        Action::Pause => Request::Pause(!panel.snapshot.local["paused"].as_bool().unwrap_or(false)),
        Action::Clear => {
            if panel
                .clear
                .is_some_and(|at| at.elapsed().as_millis() < 2500)
            {
                panel.clear = None;
                panel.clear_token += 1;
                Request::Clear
            } else {
                panel.clear = Some(Instant::now());
                panel.clear_token += 1;
                Request::ClearTimer(panel.clear_token)
            }
        }
    };
    send(&bus, &mut panel, request);
    panel.dirty = true;
}

fn age(at: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let s = now.saturating_sub(at);
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else if s < 86400 {
        format!("{}h", s / 3600)
    } else {
        format!("{}d", s / 86400)
    }
}

fn id_text(id: &Value) -> String {
    id.as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| id.to_string())
}

fn row(commands: &mut Commands, parent: Entity, entry: &Value, remote: bool, width: f32) {
    let action = if remote {
        Action::RemotePick(entry["id"].clone())
    } else {
        Action::Pick(entry["id"].clone())
    };
    let root = commands
        .spawn((
            WidgetButton,
            Interaction::default(),
            action,
            Node {
                width: percent(100),
                height: px(if remote { 30 } else { 38 }),
                flex_shrink: 0.0,
                column_gap: px(8),
                padding: UiRect::horizontal(px(10)),
                align_items: AlignItems::Center,
                overflow: Overflow::clip(),
                ..default()
            },
            BackgroundColor(colour(0x20242d)),
        ))
        .id();
    for (text, size, rgb) in [
        (id_text(&entry["id"]), 32.0, 0x8fb8e8),
        (format!("{}b", entry["bytes"]), 50.0, 0x6b7280),
        (age(entry["at"].as_u64().unwrap_or(0)), 34.0, 0x6b7280),
    ] {
        let cell = label(commands, &text, rgb, 12.0);
        commands.entity(cell).insert(Node {
            width: px(size),
            flex_shrink: 0.0,
            ..default()
        });
        commands.entity(root).add_child(cell);
    }
    let preview = entry["preview"]
        .as_str()
        .unwrap_or("")
        .replace(['\n', '\r'], " ");
    let capacity = ((width - 210.0).max(8.0) / 8.0) as usize;
    let preview = if preview.chars().count() > capacity {
        format!(
            "{}…",
            preview
                .chars()
                .take(capacity.saturating_sub(1))
                .collect::<String>()
        )
    } else {
        preview
    };
    let text = label(commands, &preview, 0xcfd3da, 13.0);
    commands.entity(text).insert((
        ctk::theme::CtkMonospace,
        Node {
            min_width: px(0),
            flex_grow: 1.0,
            overflow: Overflow::clip(),
            ..default()
        },
    ));
    commands.entity(root).add_child(text);
    commands.entity(parent).add_child(root);
}

pub fn paint(
    mut commands: Commands,
    mut panel: ResMut<Panel>,
    children: Query<&Children>,
    mut texts: Query<(&mut Text, &mut TextColor)>,
    mut backgrounds: Query<&mut BackgroundColor>,
    windows: Query<Ref<Window>>,
) {
    let resized = windows.iter().any(|w| w.is_changed());
    if !panel.dirty && !resized {
        return;
    }
    panel.dirty = false;
    let Some(view) = &panel.view else {
        return;
    };
    let s = &panel.snapshot;
    let local = &s.local;
    let paused = local["paused"].as_bool().unwrap_or(false);
    let persistence = local["persistence"].as_str().unwrap_or("?");
    let skipped = local["skipped"].as_u64().unwrap_or(0);
    let status = if s.error.is_empty() {
        format!(
            "rev {} · {} entries{} · {}{}",
            local["revision"],
            local["total"],
            if skipped > 0 {
                format!(" · {skipped} skipped")
            } else {
                String::new()
            },
            persistence,
            if paused { " · PAUSED" } else { "" }
        )
    } else {
        s.error.clone()
    };
    let armed = panel.clear.is_some();
    for (entity, value, rgb) in [
        (
            view.status,
            status,
            if persistence == "ok" && s.error.is_empty() {
                0x6b7280
            } else {
                0xd9a05b
            },
        ),
        (
            view.pause_label,
            if paused { "Resume" } else { "Pause" }.into(),
            0xcfd3da,
        ),
        (
            view.clear_label,
            if armed { "Sure?" } else { "Clear" }.into(),
            if armed { 0xffb0b0 } else { 0xcfd3da },
        ),
    ] {
        if let Ok((mut text, mut color)) = texts.get_mut(entity) {
            text.0 = value;
            color.0 = colour(rgb);
        }
    }
    if let Ok(mut bg) = backgrounds.get_mut(view.clear_button) {
        bg.0 = colour(if armed { 0x5a2626 } else { 0x20242d });
    }
    for parent in [view.table, view.remote] {
        if let Ok(children) = children.get(parent) {
            for child in children.iter() {
                commands.entity(child).despawn();
            }
        }
    }
    let width = windows.iter().next().map(|w| w.width()).unwrap_or(720.0);
    let filter = panel.filter.to_lowercase();
    for entry in &s.entries {
        if filter.is_empty()
            || entry["preview"]
                .as_str()
                .unwrap_or("")
                .to_lowercase()
                .contains(&filter)
            || id_text(&entry["id"]).to_lowercase().contains(&filter)
        {
            row(&mut commands, view.table, entry, false, width);
        }
    }
    if s.remote["ok"] == true {
        let target = s.remote["target"].as_str().unwrap_or("");
        let name = target.split('.').nth(1).unwrap_or(target);
        let title = label(&mut commands, &format!("REMOTE · {name}"), 0x6b7280, 11.0);
        commands.entity(view.remote).add_child(title);
        for entry in s.remote_entries.iter().take(4) {
            row(&mut commands, view.remote, entry, true, width);
        }
    }
}

pub fn hover(
    mut rows: Query<(&Interaction, &Action, &mut BackgroundColor), Changed<Interaction>>,
    panel: Res<Panel>,
) {
    for (interaction, action, mut bg) in &mut rows {
        let armed = matches!(action, Action::Clear) && panel.clear.is_some();
        bg.0 = colour(match (armed, interaction) {
            (true, Interaction::Hovered | Interaction::Pressed) => 0x7a3030,
            (true, _) => 0x5a2626,
            (false, Interaction::Hovered | Interaction::Pressed) => 0x262b35,
            _ => 0x20242d,
        });
    }
}
