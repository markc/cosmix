use std::collections::{BTreeMap, HashSet};

use bevy::{
    feathers::{
        dark_theme::create_dark_theme,
        theme::{ThemeBackgroundColor, ThemeTextColor, UiTheme},
    },
    prelude::*,
    text::EditableText,
    ui::InteractionDisabled,
    ui_widgets::{Activate, ScrollArea},
};
use ctk::{prelude::*, theme::tokens, tree_view::sync_tree_view};

use crate::bus::{Bus, Event, Request, Verb};

#[derive(Resource, Default)]
pub struct Browser {
    services: BTreeMap<String, Option<Result<Vec<Verb>, String>>>,
    peers: Vec<String>,
    expanded: HashSet<String>,
    selected: Option<(String, Verb)>,
    filter: String,
    dirty: bool,
    discovering: bool,
    calling: bool,
    status: String,
    peer_error: Option<String>,
    reply: String,
    detail: String,
}

#[derive(Resource)]
pub struct View {
    tree: Entity,
    search: Entity,
    body: Entity,
    call: Entity,
    detail: Entity,
    reply: Entity,
    status: Entity,
}

#[derive(Component)]
pub struct Branch(String);
#[derive(Component)]
pub struct SelectVerb(String, Verb);

fn label(commands: &mut Commands, value: &str) -> Entity {
    commands
        .spawn((
            Text::new(value),
            TextFont::from_font_size(13.0),
            ThemeTextColor(tokens::TEXT),
        ))
        .id()
}

fn pane(commands: &mut Commands) -> Entity {
    commands
        .spawn((
            Node {
                width: percent(100),
                height: percent(100),
                min_width: px(0),
                min_height: px(0),
                flex_direction: FlexDirection::Column,
                padding: UiRect::all(px(10)),
                row_gap: px(8),
                overflow: Overflow::clip(),
                ..default()
            },
            ThemeBackgroundColor(tokens::SURFACE),
        ))
        .id()
}

pub fn setup(
    mut commands: Commands,
    mut theme: ResMut<UiTheme>,
    mut theme_state: ResMut<ThemeState>,
    mut state: ResMut<Browser>,
    bus: Res<Bus>,
) {
    *theme = UiTheme(create_dark_theme());
    apply_theme(&mut theme, &mut theme_state, &ThemeSpec::builtin());
    commands.spawn(Camera2d);
    let menu = spawn_menu_bar(
        &mut commands,
        &[
            MenuDef {
                label: "File".into(),
                items: vec![
                    MenuItemDef::new("file.refresh", "Refresh"),
                    MenuItemDef::new("app.quit", "Quit"),
                ],
            },
            MenuDef {
                label: "Help".into(),
                items: vec![MenuItemDef::new("help.about", "About BusViewer")],
            },
        ],
    );
    let left = pane(&mut commands);
    let services_title = label(&mut commands, "Services on this node");
    let search_label = label(&mut commands, "Search services and verbs");
    let search = spawn_text_field(
        &mut commands,
        CtkTextFieldProps::new("", "Search services and verbs"),
    );
    commands
        .entity(search.root)
        .entry::<Node>()
        .and_modify(|mut n| n.flex_grow = 0.0);
    let tree = spawn_tree_view(&mut commands);
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
        .add_child(tree)
        .id();
    commands
        .entity(left)
        .add_children(&[services_title, search_label, search.root, scroll]);

    let right = pane(&mut commands);
    let methods_title = label(&mut commands, "Methods");
    let detail = spawn_text_area(
        &mut commands,
        CtkTextAreaProps::new("Select a verb to inspect its signature.", "Verb details")
            .read_only(true)
            .min_height(160.0)
            .visible_lines(8)
            .history_limit(0),
    );
    commands
        .entity(detail.root)
        .entry::<Node>()
        .and_modify(|mut n| {
            n.flex_grow = 0.0;
            n.height = px(230);
        });
    let body_label = label(&mut commands, "JSON body (optional)");
    let body = spawn_text_field(
        &mut commands,
        CtkTextFieldProps::new("", "JSON request body").max_length(65_536),
    );
    commands
        .entity(body.root)
        .entry::<Node>()
        .and_modify(|mut n| n.flex_grow = 0.0);
    let call = spawn_button(&mut commands, ButtonDef::text("Call").disabled());
    let controls = commands
        .spawn(Node {
            flex_shrink: 0.0,
            ..default()
        })
        .add_child(call)
        .id();
    let reply_label = label(&mut commands, "Reply");
    let reply = spawn_text_area(
        &mut commands,
        CtkTextAreaProps::new("", "Bus reply")
            .read_only(true)
            .max_len(1_048_576)
            .history_limit(0)
            .min_height(100.0)
            .visible_lines(30),
    );
    commands
        .entity(reply.root)
        .entry::<Node>()
        .and_modify(|mut n| {
            n.flex_basis = px(0);
            n.min_height = px(100);
        });
    commands.entity(right).add_children(&[
        methods_title,
        detail.root,
        body_label,
        body.root,
        controls,
        reply_label,
        reply.root,
    ]);
    let split = spawn_dcs_split(
        &mut commands,
        DcsSplitProps {
            first: left,
            second: right,
            ratio: 0.38,
        },
    );
    let status = label(&mut commands, "Connecting to the Bus…");
    commands.entity(status).insert(Node {
        padding: UiRect::axes(px(12), px(7)),
        flex_shrink: 0.0,
        ..default()
    });
    commands
        .spawn((
            Node {
                width: percent(100),
                height: percent(100),
                flex_direction: FlexDirection::Column,
                ..default()
            },
            ThemeBackgroundColor(tokens::SURFACE),
        ))
        .add_children(&[menu, split.root, status]);
    commands.insert_resource(View {
        tree,
        search: search.input,
        body: body.input,
        call,
        detail: detail.input,
        reply: reply.input,
        status,
    });
    state.detail = "Select a verb to inspect its signature.".into();
    refresh(&mut state, &bus);
}

fn refresh(state: &mut Browser, bus: &Bus) {
    if state.discovering {
        return;
    }
    match bus.requests.try_send(Request::Discover) {
        Ok(()) => {
            state.discovering = true;
            state.services.clear();
            state.peers.clear();
            state.selected = None;
            state.peer_error = None;
            state.dirty = true;
            state.status = "Connecting to the Bus…".into();
            state.detail = "Select a verb to inspect its signature.".into();
        }
        Err(error) => state.status = format!("Discovery unavailable: {error}"),
    }
}

pub fn receive(bus: Res<Bus>, mut state: ResMut<Browser>) {
    for event in bus.events.try_iter().take(256) {
        match event {
            Event::Services(names) => {
                state.services = names.into_iter().map(|name| (name, None)).collect();
                state.status = format!(
                    "Connected to the Bus — {} services; loading verbs…",
                    state.services.len()
                );
                state.dirty = true;
            }
            Event::Peers(result) => {
                match result {
                    Ok(peers) => state.peers = peers,
                    Err(error) => state.peer_error = Some(error),
                }
                state.dirty = true;
            }
            Event::Verbs(name, result) => {
                state.services.insert(name, Some(result));
                state.dirty = true;
            }
            Event::Discovered(result) => {
                state.discovering = false;
                state.status = match result {
                    Ok(()) => {
                        let failed = state
                            .services
                            .values()
                            .filter(|v| matches!(v, Some(Err(_))))
                            .count();
                        let mesh = if state.peer_error.is_some() {
                            "mesh discovery unavailable".into()
                        } else {
                            format!("{} mesh nodes", state.peers.len())
                        };
                        format!(
                            "Connected to the Bus — {} services, {mesh}{}",
                            state.services.len(),
                            if failed > 0 {
                                format!(" — {failed} introspection errors (expand to inspect)")
                            } else {
                                String::new()
                            }
                        )
                    }
                    Err(error) => format!("Discovery failed: {error} — File → Refresh to retry"),
                };
            }
            Event::Reply(reply) => {
                state.calling = false;
                state.reply = bounded_reply(reply);
            }
        }
    }
}

fn bounded_reply(reply: String) -> String {
    const LIMIT: usize = 1_000_000;
    if reply.chars().count() <= LIMIT {
        return reply;
    }
    let mut text: String = reply.chars().take(LIMIT).collect();
    text.push_str("\n\n[Reply truncated at 1,000,000 characters]");
    text
}

pub fn search(
    view: Res<View>,
    inputs: Query<&EditableText, Changed<EditableText>>,
    mut state: ResMut<Browser>,
) {
    if let Ok(input) = inputs.get(view.search) {
        let filter = input.value().to_string().to_lowercase();
        if state.filter != filter {
            state.filter = filter;
            state.dirty = true;
        }
    }
}

fn row(
    commands: &mut Commands,
    tree: Entity,
    parent: Option<Entity>,
    depth: usize,
    branch: bool,
    expanded: bool,
) -> Entity {
    let item = if branch {
        TreeItem::branch(tree, parent, expanded)
    } else {
        TreeItem::leaf(tree, parent)
    };
    let row = commands
        .spawn((
            Node {
                width: percent(100),
                min_height: px(30),
                flex_shrink: 0.0,
                align_items: AlignItems::Center,
                ..default()
            },
            item,
        ))
        .id();
    let disclosure = spawn_tree_disclosure(commands, row, depth, branch, expanded);
    commands.entity(row).add_child(disclosure);
    commands.entity(tree).add_child(row);
    row
}

pub fn rebuild_tree(
    mut commands: Commands,
    view: Res<View>,
    mut state: ResMut<Browser>,
    children: Query<&Children>,
) {
    if !state.dirty {
        return;
    }
    state.dirty = false;
    if let Ok(children) = children.get(view.tree) {
        for child in children.iter() {
            commands.entity(child).despawn();
        }
    }
    for (service, result) in &state.services {
        let service_matches = service.to_lowercase().contains(&state.filter);
        let verbs = result.as_ref().and_then(|v| v.as_ref().ok());
        let visible: Vec<&Verb> = verbs
            .into_iter()
            .flatten()
            .filter(|v| {
                service_matches
                    || format!("{} {} {}", v.name, v.args, v.description)
                        .to_lowercase()
                        .contains(&state.filter)
            })
            .collect();
        if !service_matches && visible.is_empty() {
            continue;
        }
        let expanded = state.expanded.contains(service) || !state.filter.is_empty();
        let branch = row(&mut commands, view.tree, None, 0, true, expanded);
        commands.entity(branch).insert(Branch(service.clone()));
        let title = label(&mut commands, service);
        commands.entity(branch).add_child(title);
        match result {
            Some(Ok(_)) => {
                for verb in visible {
                    let leaf = row(&mut commands, view.tree, Some(branch), 1, false, false);
                    let selected = state
                        .selected
                        .as_ref()
                        .is_some_and(|(s, v)| s == service && v.name == verb.name);
                    let button = spawn_button(
                        &mut commands,
                        ButtonDef::text(&verb.name).variant(if selected {
                            ButtonVariant::Primary
                        } else {
                            ButtonVariant::Ghost
                        }),
                    );
                    commands
                        .entity(button)
                        .insert(SelectVerb(service.clone(), verb.clone()))
                        .entry::<Node>()
                        .and_modify(|mut n| {
                            n.width = percent(100);
                            n.justify_content = JustifyContent::Start;
                        });
                    commands.entity(leaf).add_child(button);
                }
            }
            other => {
                let leaf = row(&mut commands, view.tree, Some(branch), 1, false, false);
                let text = match other {
                    Some(Err(error)) => format!("Introspection failed: {error}"),
                    _ => "Loading verbs…".into(),
                };
                let text = label(&mut commands, &text);
                commands.entity(leaf).add_child(text);
            }
        }
    }
    let mesh = row(
        &mut commands,
        view.tree,
        None,
        0,
        true,
        state.expanded.contains("@mesh"),
    );
    commands.entity(mesh).insert(Branch("@mesh".into()));
    let title = label(&mut commands, "Mesh nodes");
    commands.entity(mesh).add_child(title);
    let names = if let Some(error) = &state.peer_error {
        vec![format!("Discovery failed: {error}")]
    } else if state.peers.is_empty() {
        vec![if state.discovering {
            "Loading mesh nodes…".into()
        } else {
            "No mesh peers".into()
        }]
    } else {
        state
            .peers
            .iter()
            .filter(|name| name.to_lowercase().contains(&state.filter))
            .map(|name| format!("{name} — browsing planned"))
            .collect()
    };
    for peer in names {
        let leaf = row(&mut commands, view.tree, Some(mesh), 1, false, false);
        let text = label(&mut commands, &peer);
        commands.entity(leaf).add_child(text);
    }
    sync_tree_view(&mut commands, view.tree);
}

pub fn on_expand(event: On<TreeViewChanged>, branches: Query<&Branch>, mut state: ResMut<Browser>) {
    if let Ok(branch) = branches.get(event.item) {
        if event.expanded {
            state.expanded.insert(branch.0.clone());
        } else {
            state.expanded.remove(&branch.0);
        }
    }
}

pub fn on_activate(
    event: On<Activate>,
    view: Res<View>,
    selections: Query<&SelectVerb>,
    inputs: Query<&EditableText>,
    bus: Res<Bus>,
    mut state: ResMut<Browser>,
) {
    if let Ok(SelectVerb(service, verb)) = selections.get(event.entity) {
        state.detail = format!(
            "{}  {}\n\nArguments: {}\nRead only: {}\n\n{}",
            service,
            verb.name,
            if verb.args.is_empty() {
                "not specified"
            } else {
                &verb.args
            },
            match verb.read_only {
                Some(true) => "yes",
                Some(false) => "no",
                None => "unknown (legacy service)",
            },
            verb.description
        );
        state.selected = Some((service.clone(), verb.clone()));
        state.dirty = true;
    }
    if event.entity != view.call || state.calling {
        return;
    }
    let Some((service, verb)) = state.selected.clone() else {
        return;
    };
    let Ok(input) = inputs.get(view.body) else {
        return;
    };
    let body = input.value().to_string();
    if !body.trim().is_empty() {
        if let Err(error) = serde_json::from_str::<serde_json::Value>(&body) {
            state.reply = format!("Invalid JSON: {error}\nNo call sent.");
            return;
        }
    }
    match bus.requests.try_send(Request::Call {
        service: service.clone(),
        verb: verb.name.clone(),
        body: body.trim().into(),
    }) {
        Ok(()) => {
            state.calling = true;
            state.reply = format!("Calling {service}  {}…", verb.name);
        }
        Err(error) => state.reply = format!("Call not sent: {error}"),
    }
}

pub fn on_menu(
    event: On<MenuActivated>,
    mut state: ResMut<Browser>,
    bus: Res<Bus>,
    mut exit: MessageWriter<AppExit>,
) {
    match event.id {
        "file.refresh" => refresh(&mut state, &bus),
        "app.quit" => { exit.write(AppExit::Success); }
        "help.about" => state.reply = format!("BusViewer {}\n\nA native CTK browser and verb caller for the ABP Bus.\nExpand services, select a verb, enter an optional JSON body and press Call.\nSearch matches services and loaded verb descriptions.\nFile → Refresh reloads discovery.\nDrag the divider to resize panes; double-click to centre.\nMesh membership is listed; remote service browsing is planned.", env!("CARGO_PKG_VERSION")),
        _ => {}
    }
}

pub fn paint(
    mut commands: Commands,
    state: Res<Browser>,
    view: Res<View>,
    mut inputs: Query<&mut EditableText>,
    mut texts: Query<&mut Text>,
) {
    if !state.is_changed() {
        return;
    }
    for (entity, value) in [(view.detail, &state.detail), (view.reply, &state.reply)] {
        if let Ok(mut input) = inputs.get_mut(entity) {
            if input.value() != value.as_str() {
                input.editor_mut().set_text(value);
            }
        }
    }
    if let Ok(mut text) = texts.get_mut(view.status) {
        text.0.clone_from(&state.status);
    }
    if state.selected.is_none() || state.calling {
        commands.entity(view.call).insert(InteractionDisabled);
    } else {
        commands.entity(view.call).remove::<InteractionDisabled>();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_builds_tree_and_validates_calls_without_a_display() {
        let (requests, calls) = flume::unbounded();
        let (replies, events) = flume::unbounded();
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .add_plugins(bevy::a11y::AccessibilityPlugin)
            .add_plugins(bevy::asset::AssetPlugin::default())
            .init_asset::<bevy::shader::Shader>()
            .add_plugins(TransformPlugin)
            .add_plugins(bevy::camera::CameraPlugin)
            .add_plugins(ImagePlugin::default())
            .add_plugins(bevy::image::TextureAtlasPlugin)
            .add_plugins(bevy::mesh::MeshPlugin)
            .add_plugins(WindowPlugin {
                primary_window: None,
                ..default()
            })
            .add_plugins(bevy::input::InputPlugin)
            .add_plugins(bevy::picking::DefaultPickingPlugins)
            .add_plugins(bevy::text::TextPlugin)
            .add_plugins(bevy::input_focus::InputFocusPlugin)
            .add_plugins(bevy::input_focus::InputDispatchPlugin)
            .add_plugins(bevy::ui::UiPlugin)
            .insert_resource(Bus { requests, events });
        crate::configure(&mut app);
        app.finish();
        app.cleanup();
        app.update();
        assert!(matches!(calls.try_recv(), Ok(Request::Discover)));
        let verb = Verb {
            name: "echo".into(),
            args: "value: JSON".into(),
            description: "Echo a value".into(),
            read_only: Some(true),
        };
        replies
            .send(Event::Services(vec!["example".into()]))
            .unwrap();
        replies
            .send(Event::Verbs("example".into(), Ok(vec![verb.clone()])))
            .unwrap();
        replies.send(Event::Peers(Ok(vec!["beta".into()]))).unwrap();
        replies.send(Event::Discovered(Ok(()))).unwrap();
        app.update();
        let selection = app
            .world_mut()
            .query_filtered::<Entity, With<SelectVerb>>()
            .single(app.world())
            .unwrap();
        app.world_mut().trigger(Activate { entity: selection });
        app.update();
        assert_eq!(
            app.world()
                .resource::<Browser>()
                .selected
                .as_ref()
                .unwrap()
                .1,
            verb
        );
        let view = app.world().resource::<View>();
        let (body, call) = (view.body, view.call);
        app.world_mut()
            .get_mut::<EditableText>(body)
            .unwrap()
            .editor_mut()
            .set_text("{");
        app.world_mut().trigger(Activate { entity: call });
        assert!(calls.try_recv().is_err());
        assert!(app
            .world()
            .resource::<Browser>()
            .reply
            .contains("Invalid JSON"));
        app.world_mut()
            .get_mut::<EditableText>(body)
            .unwrap()
            .editor_mut()
            .set_text("{\"value\":42}");
        app.world_mut().trigger(Activate { entity: call });
        app.world_mut().trigger(Activate { entity: call });
        assert!(
            matches!(calls.try_recv(), Ok(Request::Call { service, verb, body }) if service == "example" && verb == "echo" && body == "{\"value\":42}")
        );
        assert!(
            calls.try_recv().is_err(),
            "an in-flight call must not be sent twice"
        );
        replies
            .send(Event::Reply("example echo\nrc = 10\n\n{}".into()))
            .unwrap();
        app.update();
        assert!(!app.world().resource::<Browser>().calling);
        assert!(app.world().resource::<Browser>().reply.contains("rc = 10"));
    }
}
