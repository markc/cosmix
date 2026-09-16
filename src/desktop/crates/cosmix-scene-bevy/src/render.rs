use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use bevy::input_focus::InputFocus;
use bevy::picking::events::{Click, Pointer};
use bevy::picking::hover::Hovered;
use bevy::prelude::*;
use bevy::text::{EditableText, EditableTextFilter, FontWeight};
use bevy::ui::Checked;
use bevy::ui_widgets::Activate;
use cosmix_scene::{Node as SceneNode, Op, ResolvedScene};
use cosmix_shell::core::Edge;
use ctk::bus::{BusBridge, BusBridgeEvent};
use ctk::button::{ButtonDef, ButtonVariant, spawn_button};
use ctk::text_area::{CtkTextArea, CtkTextAreaChanged, CtkTextAreaPlugin, CtkTextAreaSubmitted};
use ctk::text_field::{
    CtkSecretFieldProps, CtkTextFieldPlaceholder, CtkTextFieldProps, spawn_secret_field,
    spawn_text_field,
};
use ctk::virtual_list::{
    ChangeHint, RowId, VirtualListModel, VirtualListPlugin, VirtualListProps, spawn_virtual_list,
};
use ctk::widgets::{ControlChange, CtkWidgetsPlugin, toggle_button};
use serde_json::{Value, json};

use super::SceneStore;

#[derive(Component, Clone)]
struct Binding {
    scene: String,
    node: String,
    citizen: String,
    click: Option<String>,
    change: Option<String>,
    submit: Option<String>,
    item: Option<Value>,
}

impl Binding {
    fn new(scene: &ResolvedScene, id: &str, node: &SceneNode) -> Self {
        Self {
            scene: scene.name.clone(),
            node: id.into(),
            citizen: scene.citizen.clone(),
            click: string(node, "on_click"),
            change: string(node, "on_change"),
            submit: string(node, "on_submit"),
            item: None,
        }
    }
}

#[derive(Component)]
struct SceneHover {
    normal: Color,
    hover: Color,
}

#[derive(Resource, Default)]
pub struct Events {
    next: u64,
    pending: BTreeMap<u64, (String, Instant)>,
    warned: BTreeMap<String, Instant>,
}

impl Events {
    fn failed(&mut self, citizen: &str, message: &str) {
        let now = Instant::now();
        if self
            .warned
            .get(citizen)
            .is_none_or(|last| now.duration_since(*last) >= Duration::from_secs(60))
        {
            warn!("scene citizen {citizen}: {message}");
            self.warned.insert(citizen.into(), now);
        }
    }

    fn send(&mut self, bridge: &BusBridge, binding: &Binding, kind: &str, value: Option<Value>) {
        let handler = match kind {
            "click" => &binding.click,
            "change" => &binding.change,
            _ => &binding.submit,
        };
        let Some(handler) = handler else { return };
        // Bound bookkeeping even if the host stops draining replies.
        self.pending
            .retain(|_, (_, sent)| sent.elapsed() < Duration::from_secs(3));
        if self.pending.len() >= 128 {
            self.failed(&binding.citizen, "event queue full");
            return;
        }
        self.next = self.next.wrapping_add(1);
        let id = 0x53_0000_0000 | self.next;
        let mut body = json!({"scene":binding.scene,"node":binding.node,"kind":kind});
        if let Some(value) = value {
            body["value"] = value;
        }
        if let Some(item) = &binding.item {
            body["item"] = item.clone();
        }
        match bridge.try_call(
            id,
            &binding.citizen,
            handler,
            BTreeMap::new(),
            body.to_string(),
        ) {
            Ok(()) => {
                self.pending
                    .insert(id, (binding.citizen.clone(), Instant::now()));
            }
            Err(error) => self.failed(&binding.citizen, &error),
        }
    }

    pub fn reply(&mut self, event: &BusBridgeEvent) {
        if let BusBridgeEvent::Reply { request_id, result } = event
            && let Some((citizen, _)) = self.pending.remove(request_id)
        {
            match result {
                Err(error) => self.failed(&citizen, error),
                Ok(reply) if reply.rc != 0 => self.failed(&citizen, &reply.body),
                _ => {}
            }
        }
    }
}

pub(crate) fn install(app: &mut App) {
    if !app.is_plugin_added::<CtkWidgetsPlugin>() {
        app.add_plugins(CtkWidgetsPlugin);
    }
    if !app.is_plugin_added::<CtkTextAreaPlugin>() {
        app.add_plugins(CtkTextAreaPlugin);
    }
    if !app.is_plugin_added::<VirtualListPlugin>() {
        app.add_plugins(VirtualListPlugin);
    }
    app.init_resource::<Events>()
        .add_observer(activate)
        .add_observer(toggle)
        .add_observer(field_change)
        .add_observer(field_submit)
        .add_observer(row_click)
        .add_systems(Update, hover);
}

fn activate(
    event: On<Activate>,
    bindings: Query<&Binding>,
    bridge: Res<BusBridge>,
    mut events: ResMut<Events>,
) {
    if let Ok(binding) = bindings.get(event.entity) {
        events.send(&bridge, binding, "click", None);
    }
}
fn toggle(
    event: On<ControlChange>,
    bindings: Query<&Binding>,
    bridge: Res<BusBridge>,
    mut events: ResMut<Events>,
) {
    if let Ok(binding) = bindings.get(event.source) {
        events.send(&bridge, binding, "change", Some(json!(event.value != 0.0)));
    }
}
fn field_change(
    event: On<CtkTextAreaChanged>,
    bindings: Query<&Binding>,
    bridge: Res<BusBridge>,
    mut events: ResMut<Events>,
) {
    if let Ok(binding) = bindings.get(event.area) {
        events.send(&bridge, binding, "change", Some(json!(event.value)));
    }
}
fn field_submit(
    event: On<CtkTextAreaSubmitted>,
    bindings: Query<&Binding>,
    bridge: Res<BusBridge>,
    mut events: ResMut<Events>,
) {
    if let Ok(binding) = bindings.get(event.area) {
        events.send(&bridge, binding, "submit", Some(json!(event.value)));
    }
}
#[derive(Component)]
struct ClickRow;
fn row_click(
    mut event: On<Pointer<Click>>,
    bindings: Query<&Binding, With<ClickRow>>,
    bridge: Res<BusBridge>,
    mut events: ResMut<Events>,
) {
    if let Ok(binding) = bindings.get(event.entity)
        && event.button == bevy::picking::pointer::PointerButton::Primary
    {
        events.send(&bridge, binding, "click", None);
        event.propagate(false);
    }
}
fn hover(mut query: Query<(&Hovered, &SceneHover, &mut BackgroundColor), Changed<Hovered>>) {
    for (hovered, style, mut color) in &mut query {
        color.0 = if hovered.0 { style.hover } else { style.normal };
    }
}

pub(crate) struct Mounted {
    revision: u64,
    tree: ResolvedScene,
    page: Entity,
    edge: Edge,
    registered: bool,
    nodes: BTreeMap<String, View>,
}

#[cfg(feature = "gate")]
impl Mounted {
    pub(crate) fn input(&self, id: &str) -> Option<Entity> {
        self.nodes.get(id)?.input
    }
}

struct View {
    root: Entity,
    input: Option<Entity>,
    label: Option<Entity>,
    list: Option<Arc<RwLock<ListData>>>,
}

pub(crate) fn reconcile(world: &mut World) {
    world.resource_scope(|world, mut store: Mut<SceneStore>| {
        for mounted in store.removed.drain(..) {
            destroy(world, mounted);
        }
        for entry in store.scenes.values_mut() {
            let edge = scene_edge(&entry.tree);
            if entry.mounted.as_ref().is_some_and(|m| m.edge != edge) {
                // Reparent the existing page when its edge changes: fields survive.
                let m = entry.mounted.as_mut().unwrap();
                world.entity_mut(m.page).remove::<ChildOf>();
                cosmix_shell::chrome::unmount_page(world, m.edge, &page_id(&m.tree));
                m.edge = edge;
                m.registered = false;
            }
            let mounted = entry.mounted.get_or_insert_with(|| Mounted {
                revision: 0,
                tree: ResolvedScene {
                    nodes: Default::default(),
                    ..entry.tree.clone()
                },
                page: world
                    .spawn(Node {
                        width: percent(100),
                        height: percent(100),
                        min_width: px(0),
                        ..default()
                    })
                    .id(),
                edge,
                registered: false,
                nodes: BTreeMap::new(),
            });
            if mounted.revision != entry.revision {
                if mount_config(&mounted.tree) != mount_config(&entry.tree) {
                    mounted.registered = false;
                }
                apply(world, mounted, &entry.tree);
                mounted.revision = entry.revision;
            } else {
                debug_assert_eq!(mounted.tree, entry.tree);
            }
            if !mounted.registered {
                let config = mount_config(&entry.tree);
                let title = config
                    .as_ref()
                    .and_then(|w| w["title"].as_str())
                    .unwrap_or(&entry.tree.name);
                mounted.registered = cosmix_shell::chrome::mount_page(
                    world,
                    edge,
                    &page_id(&entry.tree),
                    title,
                    mounted.page,
                );
                if mounted.registered {
                    use cosmix_shell::runtime::{ShellCommand, ShellCommandKind, ShellFrameState};
                    let output = world
                        .resource::<ShellFrameState>()
                        .0
                        .geometry
                        .output
                        .clone();
                    let at = world.resource::<Time<bevy::time::Real>>().elapsed();
                    let dimension = if matches!(edge, Edge::Left | Edge::Right) {
                        "w"
                    } else {
                        "h"
                    };
                    let default_size = cosmix_shell::core::seed_panel_thickness(
                        edge,
                        world.resource::<ShellFrameState>().0.geometry.logical_size,
                    );
                    let size = config
                        .as_ref()
                        .and_then(|window| window[dimension].as_f64())
                        .map_or(default_size, |size| size.min(f32::MAX as f64) as f32);
                    cosmix_shell::runtime::set_page_thickness(world, edge, size);
                    world.write_message(ShellCommand {
                        output,
                        at,
                        kind: ShellCommandKind::Panel {
                            edge,
                            input: cosmix_shell::core::PanelInput::Reveal,
                        },
                    });
                }
            }
        }
    });
}

fn destroy(world: &mut World, mounted: Mounted) {
    cosmix_shell::chrome::unmount_page(world, mounted.edge, &page_id(&mounted.tree));
    if world.get_entity(mounted.page).is_ok() {
        world.despawn(mounted.page);
    }
}
fn page_id(tree: &ResolvedScene) -> String {
    format!("scene-{}", tree.name)
}
fn scene_edge(tree: &ResolvedScene) -> Edge {
    match mount_config(tree)
        .as_ref()
        .and_then(|w| w["edge"].as_str())
        .unwrap_or("right")
    {
        "left" => Edge::Left,
        "top" => Edge::Top,
        "bottom" => Edge::Bottom,
        _ => Edge::Right,
    }
}

fn mount_config(tree: &ResolvedScene) -> Option<Value> {
    tree.window.clone().or_else(|| {
        tree.nodes
            .values()
            .find(|node| node.family == "window")
            .map(|node| json!(node.ports))
    })
}

fn template_ids(tree: &ResolvedScene) -> BTreeSet<String> {
    fn visit(tree: &ResolvedScene, id: &str, ids: &mut BTreeSet<String>) {
        if !ids.insert(id.into()) {
            return;
        }
        if let Some(node) = tree.nodes.get(id) {
            for child in children(node) {
                visit(tree, child, ids);
            }
        }
    }
    let mut ids = BTreeSet::new();
    for id in &tree.templates {
        visit(tree, id, &mut ids);
    }
    ids
}

fn apply(world: &mut World, mounted: &mut Mounted, tree: &ResolvedScene) {
    let ops = cosmix_scene::diff(&mounted.tree, tree);
    let templates = template_ids(tree);
    // Detach scene-owned roots before removals; a retained descendant must not
    // be recursively despawned with a removed parent.
    for view in mounted.nodes.values() {
        world.entity_mut(view.root).remove::<ChildOf>();
    }
    let remove: Vec<_> = mounted
        .nodes
        .keys()
        .filter(|id| {
            templates.contains(*id)
                || tree.nodes.get(*id).is_none_or(|new| {
                    mounted.tree.nodes.get(*id).is_none_or(|old| {
                        old.family != new.family
                            || old.ports.get("password") != new.ports.get("password")
                            || (new.family == "list"
                                && (old.ports.get("row_height") != new.ports.get("row_height")
                                    || old.ports.get("gap") != new.ports.get("gap")))
                    })
                })
        })
        .cloned()
        .collect();
    for id in remove {
        if let Some(view) = mounted.nodes.remove(&id) {
            world.despawn(view.root);
        }
    }
    for (id, node) in &tree.nodes {
        if templates.contains(id) {
            continue;
        }
        let fresh = !mounted.nodes.contains_key(id);
        let view = mounted
            .nodes
            .entry(id.clone())
            .or_insert_with(|| spawn(world, tree, id, node));
        let changed = fresh
            || mounted.tree.nodes.get(id) != Some(node)
            || ops.iter().any(|op| matches!(op, Op::SetScene { .. }));
        if changed {
            update(world, tree, id, node, mounted.tree.nodes.get(id), view);
        }
        // A template edit must invalidate the list even if its own ports did not change.
        if let Some(data) = &view.list {
            *data.write().unwrap() = ListData {
                tree: tree.clone(),
                node: id.clone(),
            };
            world.trigger(ctk::virtual_list::VirtualListModelChanged {
                list: view.root,
                hint: ChangeHint::Reset,
            });
        }
    }
    // Reparent last, in authored order. Internal CTK children are untouched.
    for (id, node) in &tree.nodes {
        if let Some(parent) = mounted.nodes.get(id) {
            let entities: Vec<_> = children(node)
                .filter_map(|id| mounted.nodes.get(id).map(|v| v.root))
                .collect();
            world.entity_mut(parent.root).add_children(&entities);
        }
    }
    if let Some(root) = mounted.nodes.get("root") {
        world.entity_mut(mounted.page).add_child(root.root);
    }
    mounted.tree = tree.clone();
}

fn spawn(world: &mut World, tree: &ResolvedScene, id: &str, node: &SceneNode) -> View {
    let mut queue = bevy::ecs::world::CommandQueue::default();
    let mut commands = Commands::new(&mut queue, world);
    let mut input = None;
    let mut label = None;
    let mut list = None;
    let root = match node.family.as_str() {
        "field" => {
            let value = text(node, "value");
            let field = if flag(node, "password") {
                let field = spawn_secret_field(&mut commands, CtkSecretFieldProps::new(value, id));
                let hint = commands
                    .spawn((
                        Text::new(text(node, "placeholder")),
                        TextFont::from_font_size(13.0),
                        TextColor(Color::srgb(0.5, 0.5, 0.5)),
                        Node {
                            position_type: PositionType::Absolute,
                            left: px(7),
                            top: px(4),
                            ..default()
                        },
                        bevy::picking::Pickable::IGNORE,
                        CtkTextFieldPlaceholder { input: field.input },
                    ))
                    .id();
                commands.entity(field.input).add_child(hint);
                field
            } else {
                spawn_text_field(
                    &mut commands,
                    CtkTextFieldProps::new(value, id).placeholder(text(node, "placeholder")),
                )
            };
            commands.entity(field.input).insert((
                CtkTextArea::single_line(value, 4096),
                EditableTextFilter::new(|c| c != '\n' && c != '\r'),
            ));
            input = Some(field.input);
            field.root
        }
        "button" => spawn_button(
            &mut commands,
            ButtonDef::text(text(node, "label")).variant(tone(node)),
        ),
        "toggle" => {
            let root = commands.spawn(toggle_button(id)).id();
            let child = commands
                .spawn((
                    Text::new(text(node, "label")),
                    TextFont::from_font_size(13.0),
                ))
                .id();
            commands.entity(root).add_child(child);
            label = Some(child);
            root
        }
        "text" => {
            let child = commands
                .spawn((
                    Text::new(text(node, "text")),
                    TextFont::from_font_size(number(node, "size", 13.0)),
                    TextLayout::no_wrap(),
                ))
                .id();
            label = Some(child);
            commands.spawn(Node::default()).add_child(child).id()
        }
        "list" => {
            let model = Arc::new(RwLock::new(ListData {
                tree: tree.clone(),
                node: id.into(),
            }));
            let row_height = number(node, "row_height", 24.0) + number(node, "gap", 0.0);
            let viewport = list_height(node);
            let root = spawn_virtual_list(
                &mut commands,
                VirtualListProps::new(row_height, viewport, id),
                ListModel(model.clone()),
            )
            .root;
            list = Some(model);
            root
        }
        "image" => commands.spawn((Node::default(), ImageNode::default())).id(),
        _ => commands.spawn(Node::default()).id(),
    };
    queue.apply(world);
    View {
        root,
        input,
        label,
        list,
    }
}

fn update(
    world: &mut World,
    tree: &ResolvedScene,
    id: &str,
    node: &SceneNode,
    old: Option<&SceneNode>,
    view: &View,
) {
    let binding = Binding::new(tree, id, node);
    world
        .entity_mut(view.input.unwrap_or(view.root))
        .insert(binding);
    let mut layout = world.get::<Node>(view.root).cloned().unwrap_or_default();
    // Reset every derived constraint before applying the current resolved ports.
    layout.flex_shrink = Node::default().flex_shrink;
    layout.max_height = Val::Auto;
    layout.width = node
        .ports
        .get("width")
        .and_then(Value::as_f64)
        .map_or(Val::Auto, |v| px(v as f32));
    layout.flex_grow = if flag(node, "fill") { 1.0 } else { 0.0 };
    layout.min_width = px(0);
    layout.display = if flag(node, "hidden")
        || (node.family == "list" && flag(node, "hidden_if_empty") && rows(node).is_empty())
    {
        Display::None
    } else {
        Display::Flex
    };
    match node.family.as_str() {
        "column" | "row" => {
            layout.flex_direction = if node.family == "row" {
                FlexDirection::Row
            } else {
                FlexDirection::Column
            };
            layout.row_gap = px(number(node, "gap", 0.0));
            layout.column_gap = layout.row_gap;
            layout.padding = UiRect::all(px(number(node, "padding", 0.0)));
            layout.height = node
                .ports
                .get("height")
                .and_then(Value::as_f64)
                .map_or(Val::Auto, |v| px(v as f32));
            if node.ports.contains_key("height") {
                layout.flex_grow = 0.0;
                layout.flex_shrink = 0.0;
            }
            layout.border_radius = BorderRadius::all(px(number(node, "radius", 0.0)));
            layout.align_items = match (node.family.as_str(), text(node, "align")) {
                ("column", _) => AlignItems::Stretch,
                (_, "center") => AlignItems::Center,
                (_, "end") => AlignItems::End,
                (_, "stretch") => AlignItems::Stretch,
                _ => AlignItems::Start,
            };
            let normal = color(text(node, "background"), Color::NONE);
            world.entity_mut(view.root).insert((
                BackgroundColor(normal),
                Hovered::default(),
                SceneHover {
                    normal,
                    hover: color(text(node, "hover"), normal),
                },
            ));
            if node.ports.contains_key("on_click") {
                world.entity_mut(view.root).insert(ClickRow);
            } else {
                world.entity_mut(view.root).remove::<ClickRow>();
            }
        }
        "field" => {
            let input = view.input.unwrap();
            let focus = world.get_resource::<InputFocus>().and_then(InputFocus::get);
            if old.is_some_and(|old| old.ports.get("value") != node.ports.get("value"))
                && focus != Some(input)
                && let Some(mut editable) = world.get_mut::<EditableText>(input)
                && !editable.is_composing()
            {
                editable.editor_mut().set_text(text(node, "value"));
            }
            let hints: Vec<_> = world
                .query::<(Entity, &CtkTextFieldPlaceholder)>()
                .iter(world)
                .filter(|(_, hint)| hint.input == input)
                .map(|(e, _)| e)
                .collect();
            for hint in hints {
                world.get_mut::<Text>(hint).unwrap().0 = text(node, "placeholder").into();
            }
        }
        "button" => {
            if let Some(mut button) = world.get_mut::<ctk::button::CtkButton>(view.root) {
                button.variant = tone(node);
            }
            let children = world
                .get::<Children>(view.root)
                .map(|cs| cs.iter().collect::<Vec<_>>())
                .unwrap_or_default();
            for child in children {
                if let Some(mut label) = world.get_mut::<Text>(child) {
                    label.0 = text(node, "label").into();
                }
            }
        }
        "toggle" => {
            if flag(node, "value") {
                world.entity_mut(view.root).insert(Checked);
            } else {
                world.entity_mut(view.root).remove::<Checked>();
            }
            world.get_mut::<Text>(view.label.unwrap()).unwrap().0 = text(node, "label").into();
        }
        "text" => {
            let label = view.label.unwrap();
            world.entity_mut(label).insert((
                Text::new(text(node, "text")),
                TextFont::from_font_size(number(node, "size", 13.0)).with_font_weight(
                    if flag(node, "bold") {
                        FontWeight::BOLD
                    } else {
                        FontWeight::NORMAL
                    },
                ),
                TextColor(color(text(node, "color"), Color::WHITE)),
            ));
            if flag(node, "mono") {
                world.entity_mut(label).insert(ctk::theme::CtkMonospace);
            } else {
                world.entity_mut(label).remove::<ctk::theme::CtkMonospace>();
            }
            if flag(node, "elide") {
                world
                    .entity_mut(label)
                    .insert(ctk::text_elide::MiddleElideText::new(
                        text(node, "text"),
                        view.root,
                    ));
            } else {
                world
                    .entity_mut(label)
                    .remove::<ctk::text_elide::MiddleElideText>();
            }
        }
        "list" => {
            layout.height = px(list_height(node));
            if node.ports.contains_key("max_rows") {
                layout.max_height = layout.height;
            }
        }
        "image" => {
            let image = world
                .get_resource::<AssetServer>()
                .map(|assets| assets.load::<Image>(text(node, "src").to_owned()));
            if let Some(image) = image {
                world.entity_mut(view.root).insert(ImageNode::new(image));
            }
            layout.width = px(number(node, "w", 16.0));
            layout.height = px(number(node, "h", 16.0));
        }
        "spacer" => {
            layout.width = node
                .ports
                .get("size")
                .and_then(Value::as_f64)
                .map_or(Val::Auto, |size| px(size as f32));
            layout.height = layout.width;
            layout.flex_shrink = 0.0;
            layout.flex_grow = if layout.width == Val::Auto { 1.0 } else { 0.0 };
        }
        "window" => {
            layout.width = px(number(node, "w", 0.0));
            layout.height = px(0);
        }
        _ => {}
    }
    world.entity_mut(view.root).insert(layout);
}

struct ListData {
    tree: ResolvedScene,
    node: String,
}
struct ListModel(Arc<RwLock<ListData>>);
impl VirtualListModel for ListModel {
    fn len(&self) -> usize {
        let data = self.0.read().unwrap();
        rows(&data.tree.nodes[&data.node]).len()
    }
    fn row_id(&self, index: usize) -> RowId {
        use std::hash::{Hash, Hasher};
        let data = self.0.read().unwrap();
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        rows(&data.tree.nodes[&data.node])[index]["id"]
            .as_str()
            .unwrap_or_default()
            .hash(&mut hash);
        RowId(hash.finish())
    }
    fn bind(&self, world: &mut World, content: Entity, index: usize) {
        let data = self.0.read().unwrap();
        let node = &data.tree.nodes[&data.node];
        let Some(item) = rows(node).get(index) else {
            return;
        };
        let previous = world
            .get::<Children>(content)
            .map(|c| c.iter().collect::<Vec<_>>())
            .unwrap_or_default();
        for entity in previous {
            world.despawn(entity);
        }
        let mut binding = Binding::new(&data.tree, &data.node, node);
        binding.item = Some(item.clone());
        world.entity_mut(content).insert((binding, ClickRow));
        let template = text(node, "row");
        let root = template_node(world, &data.tree, template, item);
        world.get_mut::<Node>(root).unwrap().width = percent(100);
        world.entity_mut(content).add_child(root);
    }
}
fn template_node(world: &mut World, tree: &ResolvedScene, id: &str, item: &Value) -> Entity {
    let mut node = tree.nodes[id].clone();
    if node.family == "text" {
        let value = substitute_cells(text(&node, "text"), &item["cells"]);
        node.ports.insert("text".into(), json!(value));
    }
    let instance = format!("{id}@{}", item["id"].as_str().unwrap_or_default());
    let view = spawn(world, tree, &instance, &node);
    update(world, tree, &instance, &node, None, &view);
    world.entity_mut(view.root).insert(Name::new(instance));
    let children: Vec<_> = children(&node)
        .map(|id| template_node(world, tree, id, item))
        .collect();
    world.entity_mut(view.root).add_children(&children);
    view.root
}
fn substitute_cells(mut source: &str, cells: &Value) -> String {
    let mut out = String::new();
    while let Some(start) = source.find("{cells[") {
        out.push_str(&source[..start]);
        let token = &source[start + 7..];
        let Some(end) = token.find("]}") else {
            out.push_str(&source[start..]);
            return out;
        };
        if let Some(value) = token[..end]
            .parse::<usize>()
            .ok()
            .and_then(|index| cells.get(index))
            .and_then(Value::as_str)
        {
            out.push_str(value);
        } else {
            out.push_str(&source[start..start + 7 + end + 2]);
        }
        source = &token[end + 2..];
    }
    out.push_str(source);
    out
}
fn children(node: &SceneNode) -> impl Iterator<Item = &str> {
    node.ports
        .get("children")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
}
fn rows(node: &SceneNode) -> &[Value] {
    node.ports
        .get("rows")
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}
fn string(node: &SceneNode, port: &str) -> Option<String> {
    node.ports
        .get(port)
        .and_then(Value::as_str)
        .map(str::to_owned)
}
fn text<'a>(node: &'a SceneNode, port: &str) -> &'a str {
    node.ports
        .get(port)
        .and_then(Value::as_str)
        .unwrap_or_default()
}
fn number(node: &SceneNode, port: &str, default: f32) -> f32 {
    node.ports
        .get(port)
        .and_then(Value::as_f64)
        .map_or(default, |v| v as f32)
}
fn flag(node: &SceneNode, port: &str) -> bool {
    node.ports
        .get(port)
        .and_then(Value::as_bool)
        .unwrap_or(false)
}
fn tone(node: &SceneNode) -> ButtonVariant {
    match text(node, "tone") {
        "primary" => ButtonVariant::Primary,
        "danger" => ButtonVariant::Destructive,
        _ => ButtonVariant::default(),
    }
}
fn list_height(node: &SceneNode) -> f32 {
    (rows(node)
        .len()
        .min(number(node, "max_rows", 8.0) as usize)
        .max(1) as f32)
        * (number(node, "row_height", 24.0) + number(node, "gap", 0.0))
}
fn color(value: &str, fallback: Color) -> Color {
    bevy::color::Srgba::hex(value)
        .map(Color::Srgba)
        .unwrap_or(fallback)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn mounted(world: &mut World, tree: &ResolvedScene) -> Mounted {
        Mounted {
            revision: 0,
            tree: ResolvedScene {
                nodes: Default::default(),
                ..tree.clone()
            },
            page: world.spawn_empty().id(),
            edge: scene_edge(tree),
            registered: false,
            nodes: BTreeMap::new(),
        }
    }

    #[test]
    fn every_clearable_port_matches_a_fresh_mount() {
        let source = include_str!("../../cosmix-scene/tests/fixtures/conformance.scene.md");
        let mut checked = 0;
        let doc = cosmix_scene::parse(source).unwrap();
        for (id, node) in &doc.nodes {
            for port in node.ports.keys() {
                let mut store = SceneStore::default();
                store
                    .request(cosmix_shell::runtime::SceneVerb::Load, source, &Value::Null)
                    .unwrap();
                let mut before = store.scenes["conformance"].tree.clone();
                if store
                    .request(
                        cosmix_shell::runtime::SceneVerb::Patch,
                        "",
                        &json!({"scene":"conformance","path":format!("{id}.{port}"),"value":null}),
                    )
                    .is_err()
                {
                    continue; // Required ports and window disagreements are not clearable.
                }
                let mut after = store.scenes["conformance"].tree.clone();
                // Also render template nodes directly to cover their derived
                // constraints without relying on viewport-driven row binding.
                before.templates.clear();
                after.templates.clear();
                let after = &after;
                let mut world = World::new();
                let mut patched = mounted(&mut world, &before);
                apply(&mut world, &mut patched, &before);
                apply(&mut world, &mut patched, after);
                let mut fresh = mounted(&mut world, after);
                apply(&mut world, &mut fresh, after);
                for (key, view) in &patched.nodes {
                    assert_eq!(
                        world.get::<Node>(view.root),
                        world.get::<Node>(fresh.nodes[key].root),
                        "{id}.{port}: {key}"
                    );
                }
                checked += 1;
            }
        }
        assert!(checked > 30, "only {checked} clearable ports tested");
    }

    #[test]
    fn node_only_mount_and_edge_patch() {
        let source = "---\nscene: 1\nname: node-mount\ncitizen: test\n---\n```mix\nroot: {widget: \"window\", kind: \"edge\", edge: \"left\", title: \"Node\", w: 200}\n```\n";
        let mut store = SceneStore::default();
        store
            .request(cosmix_shell::runtime::SceneVerb::Load, source, &Value::Null)
            .unwrap();
        assert_eq!(scene_edge(&store.scenes["node-mount"].tree), Edge::Left);
        assert_eq!(
            mount_config(&store.scenes["node-mount"].tree).unwrap()["title"],
            "Node"
        );
        store
            .request(
                cosmix_shell::runtime::SceneVerb::Patch,
                "",
                &json!({"scene":"node-mount","path":"root.edge","value":"right"}),
            )
            .unwrap();
        assert_eq!(scene_edge(&store.scenes["node-mount"].tree), Edge::Right);
    }

    #[test]
    fn reconcile_moves_node_mount_and_unloads_last_page() {
        use cosmix_shell::chrome::{
            QuoinContentBindings, QuoinPageRegistry, QuoinPanelMounts, spawn_quoin_chrome,
        };
        use cosmix_shell::core::{LogicalSize, OutputKey, ShellModel};
        use cosmix_shell::runtime::{SceneVerb, ShellFrameState, ShellRuntimePlugin};
        let mut app = App::new();
        let model = ShellModel::new(
            OutputKey::new("test").unwrap(),
            LogicalSize::new(800.0, 600.0).unwrap(),
            Duration::ZERO,
            Duration::from_millis(100),
            Duration::from_millis(100),
        )
        .unwrap();
        app.add_plugins(MinimalPlugins)
            .add_plugins(ShellRuntimePlugin::new(model));
        let world = app.world_mut();
        let registry = QuoinPageRegistry::new(vec![], vec![], vec![], vec![]).unwrap();
        let props = registry
            .bind(
                &world.resource::<ShellFrameState>().0,
                QuoinContentBindings::default(),
            )
            .unwrap();
        let mounts = QuoinPanelMounts::new(
            world.spawn_empty().id(),
            world.spawn_empty().id(),
            world.spawn_empty().id(),
            world.spawn_empty().id(),
        );
        let mut queue = bevy::ecs::world::CommandQueue::default();
        spawn_quoin_chrome(&mut Commands::new(&mut queue, world), mounts, props);
        queue.apply(world);
        let mut store = SceneStore::default();
        store.request(SceneVerb::Load, "---\nscene: 1\nname: mount-test\ncitizen: test\n---\n```mix\nroot: {widget: \"window\", kind: \"edge\", edge: \"left\", w: 200}\n```\n", &Value::Null).unwrap();
        world.insert_resource(store);
        reconcile(world);
        assert_eq!(
            world
                .resource::<ShellFrameState>()
                .0
                .panel(Edge::Left)
                .page_ids
                .as_ref(),
            &["scene-mount-test"]
        );
        let page = world.resource::<SceneStore>().scenes["mount-test"]
            .mounted
            .as_ref()
            .unwrap()
            .page;
        world
            .resource_mut::<SceneStore>()
            .request(
                SceneVerb::Patch,
                "",
                &json!({"scene":"mount-test","path":"root.edge","value":"right"}),
            )
            .unwrap();
        reconcile(world);
        assert!(
            world
                .resource::<ShellFrameState>()
                .0
                .panel(Edge::Left)
                .page_ids
                .is_empty()
        );
        assert_eq!(
            world
                .resource::<ShellFrameState>()
                .0
                .panel(Edge::Right)
                .page_ids
                .as_ref(),
            &["scene-mount-test"]
        );
        assert_eq!(
            world.resource::<SceneStore>().scenes["mount-test"]
                .mounted
                .as_ref()
                .unwrap()
                .page,
            page
        );
        world
            .resource_mut::<SceneStore>()
            .request(
                SceneVerb::Patch,
                "",
                &json!({"scene":"mount-test","path":"root.w","value":null}),
            )
            .unwrap();
        reconcile(world);
        let frame = &world.resource::<ShellFrameState>().0;
        assert_eq!(
            frame.panel(Edge::Right).thickness_px,
            cosmix_shell::core::seed_panel_thickness(Edge::Right, frame.geometry.logical_size)
        );
        world
            .resource_mut::<SceneStore>()
            .request(SceneVerb::Unload, "", &json!({"scene":"mount-test"}))
            .unwrap();
        reconcile(world);
        assert!(
            world
                .resource::<ShellFrameState>()
                .0
                .panel(Edge::Right)
                .page_ids
                .is_empty()
        );
        assert!(world.get_entity(page).is_err());
    }

    #[test]
    fn unset_spacer_flexes() {
        let tree = cosmix_scene::resolve(&cosmix_scene::parse("---\nscene: 1\nname: spacer-test\ncitizen: test\n---\n```mix\nroot: {widget: \"spacer\"}\n```\n").unwrap()).unwrap();
        let mut world = World::new();
        let mut mounted = mounted(&mut world, &tree);
        apply(&mut world, &mut mounted, &tree);
        let layout = world.get::<Node>(mounted.nodes["root"].root).unwrap();
        assert_eq!(layout.width, Val::Auto);
        assert_eq!(layout.flex_grow, 1.0);
    }
    #[test]
    fn cell_values_are_literal_and_not_reexpanded() {
        assert_eq!(
            substitute_cells("{cells[0]} / {cells[1]}", &json!(["{cells[1]}", "literal"])),
            "{cells[1]} / literal"
        );
    }
    #[test]
    fn unrelated_reload_retains_the_entire_field_entity() {
        let doc = cosmix_scene::parse(include_str!(
            "../../cosmix-scene/tests/fixtures/conformance.scene.md"
        ))
        .unwrap();
        let tree = cosmix_scene::resolve(&doc).unwrap();
        let mut world = World::new();
        let page = world.spawn_empty().id();
        let mut mounted = Mounted {
            revision: 0,
            tree: ResolvedScene {
                nodes: Default::default(),
                ..tree.clone()
            },
            page,
            edge: Edge::Left,
            registered: false,
            nodes: BTreeMap::new(),
        };
        apply(&mut world, &mut mounted, &tree);
        let input = mounted.nodes["field"].input.unwrap();
        world.insert_resource(InputFocus::from_entity(input));
        world.get_mut::<EditableText>(input).unwrap().queue_edit(
            bevy::text::TextEdit::ImeSetCompose {
                value: "pending".into(),
                cursor: None,
            },
        );
        let before = format!("{:?}", world.get::<CtkTextArea>(input).unwrap());
        let mut changed = tree.clone();
        changed.nodes["text"]
            .ports
            .insert("text".into(), json!("new status"));
        apply(&mut world, &mut mounted, &changed);
        assert_eq!(mounted.nodes["field"].input, Some(input));
        assert_eq!(world.resource::<InputFocus>().get(), Some(input));
        assert_eq!(
            format!("{:?}", world.get::<CtkTextArea>(input).unwrap()),
            before
        );
        assert!(world.get::<EditableText>(input).unwrap().pending_edits.iter().any(|edit| matches!(edit, bevy::text::TextEdit::ImeSetCompose { value, .. } if value == "pending")));
    }
}
