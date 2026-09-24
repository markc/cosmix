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

mod icons;
#[cfg(test)]
mod layout_tests;
use icons::IconCache;

#[derive(Resource)]
struct IconScale(f32);

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
        .init_resource::<IconCache>()
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

// Restore CTK-owned defaults when an explicit constraint is cleared. Reading
// the previous Node would otherwise retain the last authored min/max/shrink.
#[derive(Component)]
struct SceneLayoutBase(Node);

/// Apply accepted scene transactions to mounted chrome. `ScenePlugin` schedules
/// this between shell input and model updates; minimal hosts may do so directly.
/// Mounting only registers content and seeds an unset edge size. Reveal and
/// selection remain explicit shell actions, including on scene revisions.
pub fn reconcile(world: &mut World) {
    remove_unseated_scenes(world);
    let scale = icons::effective_scale(world);
    let scale_changed = world
        .get_resource::<IconScale>()
        .is_none_or(|old| old.0 != scale);
    world.insert_resource(IconScale(scale));
    world.resource_scope(|world, mut store: Mut<SceneStore>| {
        for mounted in store.removed.drain(..) {
            destroy(world, mounted);
        }
        for entry in store.scenes.values_mut() {
            let edge = scene_edge(&entry.tree);
            // Production ingress reserved this exact name before replying.
            // Rendering never creates or steals a registry seat.
            if let Some(owner) = &entry.owner {
                let valid = world
                    .get_resource::<cosmix_shell::runtime::SubPanelRegistryState>()
                    .and_then(|registry| registry.0.seat(&page_id(&entry.tree)))
                    .is_some_and(|seat| {
                        seat.owner == owner.citizen
                            && seat.accepted_at == owner.accepted_at
                            && seat.edge == edge
                            && world
                                .get_resource::<cosmix_shell::runtime::ShellFrameState>()
                                .is_some_and(|frame| seat.output == frame.0.geometry.output)
                    });
                if !valid {
                    warn!(
                        scene = entry.tree.name,
                        "scene mount has no matching reserved seat"
                    );
                    continue;
                }
            }
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
            if mounted.revision != entry.revision || scale_changed {
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
                mounted.registered = cosmix_shell::chrome::mount_page_with(
                    world,
                    edge,
                    &page_id(&entry.tree),
                    title,
                    mounted.page,
                    config.as_ref().and_then(|w| w["chrome"].as_bool()) == Some(false),
                );
                if mounted.registered {
                    use cosmix_shell::runtime::ShellFrameState;
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
                    cosmix_shell::runtime::seed_page_thickness(world, edge, size);
                }
            }
        }
    });
}

/// Registry removal owns the carousel landing. Drop its scene and chrome in
/// the same update, without issuing another carousel removal. Unowned test
/// and minimal-host scenes have no seat and follow explicit unload instead.
pub(crate) fn remove_unseated_scenes(world: &mut World) {
    world.resource_scope(|world, mut store: Mut<SceneStore>| {
        let Some(registry) = world.get_resource::<cosmix_shell::runtime::SubPanelRegistryState>()
        else {
            return;
        };
        let removed: Vec<_> = store
            .scenes
            .iter()
            .filter_map(|(name, entry)| {
                entry
                    .owner
                    .as_ref()
                    .filter(|owner| {
                        registry.0.seat(&page_id(&entry.tree)).is_none_or(|seat| {
                            seat.owner != owner.citizen || seat.accepted_at != owner.accepted_at
                        })
                    })
                    .map(|_| name.clone())
            })
            .collect();
        for name in removed {
            if let Some(mounted) = store.scenes.remove(&name).and_then(|entry| entry.mounted) {
                let id = page_id(&mounted.tree);
                let still_registered = world
                    .get_resource::<cosmix_shell::runtime::ShellFrameState>()
                    .is_some_and(|frame| frame.0.panel(mounted.edge).page_ids.contains(&id));
                if still_registered {
                    eprintln!("QUOIN_INVARIANT unseated_scene_still_in_carousel page={id}");
                    #[cfg(test)]
                    debug_assert!(!still_registered, "unseated scene still in carousel: {id}");
                }
                cosmix_shell::chrome::unmount_page_content(
                    world,
                    mounted.edge,
                    &page_id(&mounted.tree),
                );
                if world.get_entity(mounted.page).is_ok() {
                    world.despawn(mounted.page);
                }
            }
        }
    });
}

fn destroy(world: &mut World, mounted: Mounted) {
    let id = page_id(&mounted.tree);
    // The acceptance/unload transaction owns the reservation. An old mounted
    // tree can be destroyed after a same-name replacement has reserved it.
    // Explicit unload lands here; registry-driven removal instead tears down
    // content through remove_unseated_scenes without a second carousel remove.
    cosmix_shell::chrome::unmount_page(world, mounted.edge, &id);
    if world.get_entity(mounted.page).is_ok() {
        world.despawn(mounted.page);
    }
}
/// The sub-panel address a scene mounts under.
///
/// The default is the anonymous `scene-<name>` id. A document authored for a
/// *declared* sub-panel (panel doc §2/§5) names it in the window envelope's
/// `panel` field — mounting metadata beside `edge`/`title` — because a
/// declared name is the carousel's address and can carry characters the
/// scene-name grammar forbids (`settings.appearance`). Without the override a
/// declared slot could never be filled by scene content.
pub(crate) fn page_id(tree: &ResolvedScene) -> String {
    mount_config(tree)
        .as_ref()
        .and_then(|window| window["panel"].as_str())
        .filter(|name| !name.trim().is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("scene-{}", tree.name))
}
pub(crate) fn scene_edge(tree: &ResolvedScene) -> Edge {
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
    icons::begin_revision(world);
    let ops = cosmix_scene::diff(&mounted.tree, tree);
    let templates = template_ids(tree);
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
        .collect::<Vec<_>>();
    // The hierarchy is only rebuilt when it can have changed: a node removed
    // or added, or any node's children list edited. Detaching and re-parenting
    // every scene root on an otherwise unchanged revision makes Bevy re-lay
    // out every text, blanking all labels for a few frames -- on a panel that
    // re-renders each minute that is a visible flicker of the whole bar.
    let structural = !remove.is_empty()
        || tree
            .nodes
            .keys()
            .any(|id| !templates.contains(id) && !mounted.nodes.contains_key(id))
        || tree.nodes.iter().any(|(id, node)| {
            mounted
                .tree
                .nodes
                .get(id)
                .is_none_or(|old| children(old).ne(children(node)))
        });
    if structural {
        // Detach scene-owned roots before removals; a retained descendant must
        // not be recursively despawned with a removed parent.
        for view in mounted.nodes.values() {
            world.entity_mut(view.root).remove::<ChildOf>();
        }
    }
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
        // A fill text's cross-axis stretch depends on its PARENT column, so a
        // changed parent re-updates it; an unchanged text is otherwise left
        // alone -- re-inserting its Text components forces a re-layout that
        // blanks the label for a frame on every revision (a visible flicker
        // on a panel that re-renders each minute). Images still re-update on
        // every apply: that is how a missing icon file is retried on the next
        // revision, and a cache hit re-inserts the same handle (no flicker).
        let text_parent_changed = node.family == "text"
            && flag(node, "fill")
            && tree.nodes.iter().any(|(parent_id, parent)| {
                children(parent).any(|child| child == id)
                    && mounted.tree.nodes.get(parent_id) != Some(parent)
            });
        let view = mounted
            .nodes
            .entry(id.clone())
            .or_insert_with(|| spawn(world, tree, id, node));
        let changed = fresh
            || node.family == "image"
            || text_parent_changed
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
    if structural {
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
    let base = world.get::<Node>(root).cloned().unwrap_or_default();
    world.entity_mut(root).insert(SceneLayoutBase(base));
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
    let base = &world.get::<SceneLayoutBase>(view.root).unwrap().0;
    // Leave native controls untouched unless the scene previously owned the
    // property. In particular, a legacy reload must not reset CTK's metrics.
    if old.is_some_and(|old| old.ports.contains_key("shrink")) {
        layout.flex_shrink = base.flex_shrink;
    }
    if old.is_some_and(|old| old.ports.contains_key("basis")) {
        layout.flex_basis = base.flex_basis;
    }
    if old.is_some_and(|old| old.ports.contains_key("align_self")) {
        layout.align_self = base.align_self;
    }
    for (port, target, baseline) in [
        ("min_height", &mut layout.min_height, base.min_height),
        ("max_width", &mut layout.max_width, base.max_width),
        ("max_height", &mut layout.max_height, base.max_height),
    ] {
        if old.is_some_and(|old| old.ports.contains_key(port)) {
            *target = baseline;
        }
    }
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
            // Clear the height-derived constraint while preserving CTK-owned
            // baseline constraints on other widget families.
            layout.flex_shrink = Node::default().flex_shrink;
            layout.flex_direction = if node.family == "row" {
                FlexDirection::Row
            } else {
                FlexDirection::Column
            };
            layout.row_gap = px(number(node, "row_gap", number(node, "gap", 0.0)));
            layout.column_gap = px(number(node, "column_gap", number(node, "gap", 0.0)));
            layout.padding = UiRect {
                top: px(number(node, "padding_top", number(node, "padding", 0.0))),
                right: px(number(node, "padding_right", number(node, "padding", 0.0))),
                bottom: px(number(node, "padding_bottom", number(node, "padding", 0.0))),
                left: px(number(node, "padding_left", number(node, "padding", 0.0))),
            };
            layout.justify_content = match text(node, "justify") {
                "start" => JustifyContent::Start,
                "center" => JustifyContent::Center,
                "end" => JustifyContent::End,
                "between" => JustifyContent::SpaceBetween,
                "around" => JustifyContent::SpaceAround,
                "evenly" => JustifyContent::SpaceEvenly,
                _ => JustifyContent::Default,
            };
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
            layout.align_items = match text(node, "align") {
                "center" => AlignItems::Center,
                "end" => AlignItems::End,
                "stretch" => AlignItems::Stretch,
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
            world.get_mut::<TextLayout>(label).unwrap().justify = match text(node, "align") {
                "center" => Justify::Center,
                "right" => Justify::Right,
                _ => Justify::Left,
            };
            // Bevy 0.19's NoWrap path discards the node width and shapes with
            // TextBounds::UNBOUNDED. Keep the single-line label intrinsic and
            // position its box with Taffy inside the allocated wrapper instead.
            // TextLayout::justify still aligns explicit newline-separated lines.
            layout.justify_content = match text(node, "align") {
                "center" => JustifyContent::Center,
                "right" => JustifyContent::End,
                _ => JustifyContent::Start,
            };
            // Keep the legacy zero wrapper minimum set above. P1's auto
            // minimum changed panel allocation independently of centring;
            // authors can still override it with the explicit min_width port.
            {
                let mut label_node = world.get_mut::<Node>(label).unwrap();
                label_node.width = Val::Auto;
                label_node.min_width = Val::Auto;
                label_node.flex_shrink = 0.0;
            }
            if flag(node, "fill")
                && tree.nodes.values().any(|parent| {
                    parent.family == "column"
                        && text(parent, "align") == "stretch"
                        && children(parent).any(|child| child == id.split('@').next().unwrap_or(id))
                })
            {
                layout.align_self = AlignSelf::Stretch;
            } else {
                layout.align_self = AlignSelf::Auto;
            }
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
            layout.max_height = if node.ports.contains_key("max_rows") {
                layout.height
            } else {
                Val::Auto
            };
        }
        "image" => {
            let src = text(node, "src");
            let image = if src.starts_with('/') {
                icons::load(world, src, number(node, "w", 16.0), number(node, "h", 16.0))
            } else {
                world
                    .get_resource::<AssetServer>()
                    .map(|assets| assets.load::<Image>(src.to_owned()))
            };
            if let Some(image) = image {
                world.entity_mut(view.root).insert(ImageNode::new(image));
            } else if src.starts_with('/') {
                world.entity_mut(view.root).remove::<ImageNode>();
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
    // Explicit values override only their own legacy/default values. The
    // schema rejects ambiguous fill + explicit flex declarations at ingress.
    if let Some(value) = node.ports.get("grow").and_then(Value::as_f64) {
        layout.flex_grow = value as f32;
    }
    if let Some(value) = node.ports.get("shrink").and_then(Value::as_f64) {
        layout.flex_shrink = value as f32;
    }
    if let Some(value) = node.ports.get("basis").and_then(Value::as_f64) {
        layout.flex_basis = px(value as f32);
    }
    if node.ports.contains_key("align_self") {
        layout.align_self = match text(node, "align_self") {
            "start" => AlignSelf::Start,
            "center" => AlignSelf::Center,
            "end" => AlignSelf::End,
            "stretch" => AlignSelf::Stretch,
            _ => AlignSelf::Auto,
        };
    }
    for (port, target) in [
        ("min_width", &mut layout.min_width),
        ("max_width", &mut layout.max_width),
        ("min_height", &mut layout.min_height),
        ("max_height", &mut layout.max_height),
    ] {
        if let Some(value) = node.ports.get(port).and_then(Value::as_f64) {
            *target = px(value as f32);
        }
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
    } else if node.family == "image" {
        let value = substitute_cells(text(&node, "src"), &item["cells"]);
        node.ports.insert("src".into(), json!(value));
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
    use bevy::ecs::message::Messages;

    fn mount_test_app(chrome: bool) -> App {
        use cosmix_shell::core::{LogicalSize, OutputKey, ShellModel};
        use cosmix_shell::runtime::{ShellRuntimePlugin, ShellRuntimeSet};
        let model = ShellModel::new(
            OutputKey::new("test").unwrap(),
            LogicalSize::new(800.0, 600.0).unwrap(),
            Duration::ZERO,
            Duration::from_millis(100),
            Duration::from_millis(100),
        )
        .unwrap();
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, ShellRuntimePlugin::new(model)))
            .init_resource::<SceneStore>()
            .add_systems(
                Update,
                remove_unseated_scenes
                    .after(ShellRuntimeSet::Model)
                    .before(ShellRuntimeSet::Presentation),
            );
        if chrome {
            mount_test_chrome(app.world_mut());
        }
        app
    }

    fn mount_test_chrome(world: &mut World) {
        use cosmix_shell::chrome::{
            QuoinContentBindings, QuoinPageContent, QuoinPageRegistry, QuoinPageSpec,
            QuoinPanelMounts, spawn_quoin_chrome,
        };
        use cosmix_shell::runtime::ShellFrameState;
        // Late chrome construction must bind the pages already in the model.
        let frame = world.resource::<ShellFrameState>().0.clone();
        let mut bindings = QuoinContentBindings::default();
        let specs = Edge::ALL.map(|edge| {
            let ids = &frame.panel(edge).page_ids;
            bindings.set(
                edge,
                ids.iter()
                    .map(|id| QuoinPageContent::new(id.clone(), world.spawn_empty().id()))
                    .collect(),
            );
            ids.iter()
                .map(|id| QuoinPageSpec::new(id.clone(), id.clone()))
                .collect()
        });
        let [left, bottom, right, top] = specs;
        let props = QuoinPageRegistry::new(left, bottom, right, top)
            .unwrap()
            .bind(&frame, bindings)
            .unwrap();
        let mounts = QuoinPanelMounts::new(
            world.spawn_empty().id(),
            world.spawn_empty().id(),
            world.spawn_empty().id(),
            world.spawn_empty().id(),
        );
        spawn_quoin_chrome(&mut world.commands(), mounts, props);
        world.flush();
    }

    fn load_mount_test_scene(world: &mut World, name: &str, edge: Edge, size: u32) {
        use cosmix_shell::runtime::{SceneVerb, ShellFrameState, SubPanelRegistryState};
        let edge = match edge {
            Edge::Left => "left",
            Edge::Right => "right",
            Edge::Top => "top",
            Edge::Bottom => "bottom",
        };
        let source = format!(
            "---\nscene: 1\nname: {name}\ncitizen: test\n---\n```mix\nroot: {{widget: \"window\", kind: \"edge\", edge: \"{edge}\", w: {size}, h: {size}}}\n```\n"
        );
        world.resource_scope(|world, mut store: Mut<SceneStore>| {
            world.resource_scope(|world, mut registry: Mut<SubPanelRegistryState>| {
                store
                    .request_mounted(
                        SceneVerb::Load,
                        &source,
                        &Value::Null,
                        Some(&mut super::super::SceneMount {
                            registry: &mut registry.0,
                            output: &world.resource::<ShellFrameState>().0.geometry.output,
                            owner: "test",
                            accepted_at: 1,
                        }),
                    )
                    .unwrap();
            });
        });
    }

    #[test]
    fn first_mount_registers_without_reveal_or_selection() {
        use cosmix_shell::runtime::{ShellCommand, ShellFrameState, set_shell_pages};
        let mut app = mount_test_app(true);
        let world = app.world_mut();
        set_shell_pages(world, Edge::Left, vec!["existing".into()], Some("existing"));
        load_mount_test_scene(world, "new", Edge::Left, 200);
        reconcile(world);
        assert!(world.resource::<Messages<ShellCommand>>().is_empty());
        app.update();
        let panel = app
            .world()
            .resource::<ShellFrameState>()
            .0
            .panel(Edge::Left);
        assert_eq!(panel.page_ids.as_ref(), ["existing", "scene-new"]);
        assert_eq!(panel.active_page_id.as_deref(), Some("existing"));
        assert!(!panel.mapped);
    }

    #[test]
    fn mount_seeds_thickness_only_when_unset() {
        use cosmix_shell::runtime::{ShellFrameState, set_page_thickness};
        for edge in Edge::ALL {
            for remembered in [false, true] {
                let mut app = mount_test_app(true);
                let world = app.world_mut();
                if remembered {
                    set_page_thickness(world, edge, 173.0);
                }
                load_mount_test_scene(world, "first", edge, 210);
                reconcile(world);
                let expected = if remembered { 173.0 } else { 210.0 };
                assert_eq!(
                    world
                        .resource::<ShellFrameState>()
                        .0
                        .panel(edge)
                        .thickness_px,
                    expected
                );
                load_mount_test_scene(world, "second", edge, 290);
                reconcile(world);
                assert_eq!(
                    world
                        .resource::<ShellFrameState>()
                        .0
                        .panel(edge)
                        .thickness_px,
                    expected
                );
            }
        }
    }

    #[test]
    fn remount_after_revision_preserves_thickness_and_selection() {
        use cosmix_shell::runtime::{
            SceneVerb, ShellFrameState, set_page_thickness, set_shell_pages,
        };
        let mut app = mount_test_app(true);
        let world = app.world_mut();
        load_mount_test_scene(world, "revision", Edge::Left, 200);
        reconcile(world);
        let page = world.resource::<SceneStore>().scenes["revision"]
            .mounted
            .as_ref()
            .unwrap()
            .page;
        set_shell_pages(
            world,
            Edge::Left,
            vec!["scene-revision".into(), "selected".into()],
            Some("selected"),
        );
        set_page_thickness(world, Edge::Left, 177.0);
        for (path, value) in [
            ("root.w", json!(310)),
            ("root.title", json!("Revised")),
            ("root.chrome", json!(false)),
        ] {
            world
                .resource_mut::<SceneStore>()
                .request(
                    SceneVerb::Patch,
                    "",
                    &json!({"scene":"revision", "path":path, "value":value}),
                )
                .unwrap();
            reconcile(world);
            let panel = world.resource::<ShellFrameState>().0.panel(Edge::Left);
            assert_eq!(panel.thickness_px, 177.0);
            assert_eq!(panel.active_page_id.as_deref(), Some("selected"));
            assert_eq!(
                world.resource::<SceneStore>().scenes["revision"]
                    .mounted
                    .as_ref()
                    .unwrap()
                    .page,
                page
            );
        }
    }

    #[test]
    fn late_registration_while_chrome_absent_retries() {
        use cosmix_shell::runtime::{
            ShellCommand, ShellFrameState, set_page_thickness, set_shell_pages,
        };
        let mut app = mount_test_app(false);
        let world = app.world_mut();
        set_shell_pages(world, Edge::Left, vec!["selected".into()], Some("selected"));
        set_page_thickness(world, Edge::Left, 177.0);
        load_mount_test_scene(world, "late", Edge::Left, 200);
        reconcile(world);
        reconcile(world);
        let mounted = world.resource::<SceneStore>().scenes["late"]
            .mounted
            .as_ref()
            .unwrap();
        assert!(!mounted.registered);
        let page = mounted.page;
        mount_test_chrome(world);
        reconcile(world);
        let mounted = world.resource::<SceneStore>().scenes["late"]
            .mounted
            .as_ref()
            .unwrap();
        assert!(mounted.registered);
        assert_eq!(mounted.page, page);
        let panel = world.resource::<ShellFrameState>().0.panel(Edge::Left);
        assert_eq!(panel.page_ids.as_ref(), ["selected", "scene-late"]);
        assert_eq!(panel.active_page_id.as_deref(), Some("selected"));
        assert_eq!(panel.thickness_px, 177.0);
        assert!(world.resource::<Messages<ShellCommand>>().is_empty());
    }

    #[test]
    fn mount_preserves_verb_registered_pages() {
        use cosmix_shell::runtime::{
            ShellCommand, ShellCommandKind, ShellFrameState, SubPanelRegistryState,
        };
        let mut app = mount_test_app(true);
        let world = app.world_mut();
        let output = world
            .resource::<ShellFrameState>()
            .0
            .geometry
            .output
            .clone();
        world
            .resource_mut::<SubPanelRegistryState>()
            .0
            .mount("verb-only", output.clone(), Edge::Left, "owner", 1)
            .unwrap();
        world.write_message(ShellCommand {
            output,
            at: Duration::ZERO,
            kind: ShellCommandKind::SubPanelRegister {
                edge: Edge::Left,
                name: "verb-only".into(),
                owner: "owner".into(),
            },
        });
        app.update();
        let world = app.world_mut();
        load_mount_test_scene(world, "mounted", Edge::Left, 200);
        reconcile(world);
        assert_eq!(
            world
                .resource::<ShellFrameState>()
                .0
                .panel(Edge::Left)
                .page_ids
                .as_ref(),
            ["verb-only", "scene-mounted"]
        );
        assert!(
            world
                .resource::<SubPanelRegistryState>()
                .0
                .seat("verb-only")
                .is_some()
        );
    }

    #[test]
    #[should_panic(expected = "unseated scene still in carousel")]
    fn unseated_scene_with_live_carousel_page_trips_invariant() {
        use cosmix_shell::runtime::SubPanelRegistryState;
        let mut app = mount_test_app(true);
        let world = app.world_mut();
        load_mount_test_scene(world, "orphan", Edge::Left, 200);
        reconcile(world);
        // Deliberately violate the registry-removal contract: drop only the
        // seat, leaving the frame's page live when content teardown runs.
        world.resource_mut::<SubPanelRegistryState>().0.forget("scene-orphan");
        remove_unseated_scenes(world);
    }

    #[test]
    fn sub_remove_scene_backed_unloads_content_once() {
        use cosmix_shell::runtime::{
            SceneVerb, ShellCommand, ShellCommandKind, ShellFrameState, SubPanelRegistryState,
            set_shell_pages,
        };
        let mut app = mount_test_app(true);
        let world = app.world_mut();
        load_mount_test_scene(world, "removed", Edge::Left, 200);
        let output = world
            .resource::<ShellFrameState>()
            .0
            .geometry
            .output
            .clone();
        reconcile(world);
        let page = world.resource::<SceneStore>().scenes["removed"]
            .mounted
            .as_ref()
            .unwrap()
            .page;
        let wrapper = world.get::<ChildOf>(page).unwrap().parent();
        set_shell_pages(
            world,
            Edge::Left,
            vec!["primary".into(), "previous".into(), "scene-removed".into()],
            Some("scene-removed"),
        );
        world.write_message(ShellCommand {
            output,
            at: Duration::ZERO,
            kind: ShellCommandKind::SubPanelRemove {
                edge: Edge::Left,
                name: "scene-removed".into(),
                owner: "test".into(),
                accepted_at: 1,
            },
        });
        app.update();
        let world = app.world_mut();
        assert!(
            !world
                .resource::<SceneStore>()
                .scenes
                .contains_key("removed")
        );
        assert!(
            world
                .resource::<SubPanelRegistryState>()
                .0
                .seat("scene-removed")
                .is_none()
        );
        assert!(world.get_entity(page).is_err());
        assert!(world.get_entity(wrapper).is_err());
        assert!(
            world
                .resource_mut::<SceneStore>()
                .request(SceneVerb::Get, "", &json!({"scene":"removed"}))
                .is_err()
        );
        load_mount_test_scene(world, "later", Edge::Left, 300);
        reconcile(world);
        reconcile(world);
        let panel = world.resource::<ShellFrameState>().0.panel(Edge::Left);
        assert_eq!(
            panel.page_ids.as_ref(),
            ["primary", "previous", "scene-later"]
        );
        assert_eq!(panel.active_page_id.as_deref(), Some("previous"));
        // The removal landing is distinct from remembered selection: the
        // next reveal must return to the primary, even after another mount.
        let output = world
            .resource::<ShellFrameState>()
            .0
            .geometry
            .output
            .clone();
        let at = world.resource::<Time<bevy::time::Real>>().elapsed();
        world.write_message(ShellCommand {
            output,
            at,
            kind: ShellCommandKind::Panel {
                edge: Edge::Left,
                input: cosmix_shell::core::PanelInput::Reveal,
            },
        });
        app.update();
        assert_eq!(
            app.world()
                .resource::<ShellFrameState>()
                .0
                .panel(Edge::Left)
                .active_page_id
                .as_deref(),
            Some("primary"),
        );
    }

    const ICON_SVG: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" width="8" height="8"><rect width="8" height="8" fill="red" fill-opacity="0.5"/></svg>"#;

    fn icon_tree(path: &std::path::Path) -> ResolvedScene {
        let source = format!(
            "---\nscene: 1\nname: icons\ncitizen: test\n---\n```mix\nroot: {{widget: \"image\", src: {}, w: 24, h: 24}}\n```\n",
            json!(path.to_str().unwrap())
        );
        cosmix_scene::resolve(&cosmix_scene::parse(&source).unwrap()).unwrap()
    }

    fn rendered_icon(world: &World, entity: Entity, pixels: u32) -> Handle<Image> {
        let handle = world.get::<ImageNode>(entity).unwrap().image.clone();
        let image = world.resource::<Assets<Image>>().get(&handle).unwrap();
        assert_eq!(image.texture_descriptor.size.width, pixels);
        assert_eq!(image.texture_descriptor.size.height, pixels);
        handle
    }

    #[test]
    fn svg_icon_rasterises_at_oversampled_size() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("icon.SVG");
        std::fs::write(&path, ICON_SVG).unwrap();
        let tree = icon_tree(&path);
        let mut world = World::new();
        let mut window = Window::default();
        window.resolution.set_scale_factor_override(Some(2.5));
        world.spawn((window, bevy::window::PrimaryWindow));
        let mut mounted = mounted(&mut world, &tree);
        apply(&mut world, &mut mounted, &tree);
        let handle = rendered_icon(&world, mounted.nodes["root"].root, 60);
        let image = world.resource::<Assets<Image>>().get(&handle).unwrap();
        assert_eq!(&image.data.as_ref().unwrap()[..4], &[255, 0, 0, 128]);
    }

    #[test]
    fn png_icon_loads_from_absolute_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("icon.png");
        image::RgbaImage::from_pixel(8, 8, image::Rgba([0, 255, 0, 255]))
            .save(&path)
            .unwrap();
        let tree = icon_tree(&path);
        let mut world = World::new();
        let mut mounted = mounted(&mut world, &tree);
        apply(&mut world, &mut mounted, &tree);
        rendered_icon(&world, mounted.nodes["root"].root, 24);
    }

    #[test]
    fn missing_icon_is_absent_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.svg");
        let tree = icon_tree(&path);
        let mut world = World::new();
        let mut mounted = mounted(&mut world, &tree);
        apply(&mut world, &mut mounted, &tree);
        let view = &mounted.nodes["root"];
        assert!(world.get::<ImageNode>(view.root).is_none());
        // A now-valid file stays absent: the second update must use the negative cache.
        std::fs::write(&path, ICON_SVG).unwrap();
        update(&mut world, &tree, "root", &tree.nodes["root"], None, view);
        assert!(world.get::<ImageNode>(view.root).is_none());
        apply(&mut world, &mut mounted, &tree);
        rendered_icon(&world, mounted.nodes["root"].root, 24);
    }

    #[test]
    fn scale_change_reapplies_unchanged_scene_icons() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("icon.svg");
        std::fs::write(&path, ICON_SVG).unwrap();
        let source = format!(
            "---\nscene: 1\nname: icons\ncitizen: test\n---\n```mix\nroot: {{widget: \"image\", src: {}, w: 24, h: 24}}\n```\n",
            json!(path.to_str().unwrap())
        );
        let mut store = SceneStore::default();
        store
            .request(
                cosmix_shell::runtime::SceneVerb::Load,
                &source,
                &Value::Null,
            )
            .unwrap();
        let mut world = World::new();
        world.insert_resource(store);
        reconcile(&mut world);
        let entry = &world.resource::<SceneStore>().scenes["icons"];
        let revision = entry.revision;
        let root = entry.mounted.as_ref().unwrap().nodes["root"].root;
        let first = rendered_icon(&world, root, 24);
        world.insert_resource(UiScale(2.0));
        reconcile(&mut world);
        assert_ne!(rendered_icon(&world, root, 48), first);
        assert_eq!(
            world.resource::<SceneStore>().scenes["icons"].revision,
            revision
        );
    }

    #[test]
    fn icon_cache_dedupes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("icon.svg");
        std::fs::write(&path, ICON_SVG).unwrap();
        let tree = icon_tree(&path);
        let mut world = World::new();
        let mut first = mounted(&mut world, &tree);
        apply(&mut world, &mut first, &tree);
        let handle = rendered_icon(&world, first.nodes["root"].root, 24);
        let mut second = mounted(&mut world, &tree);
        apply(&mut world, &mut second, &tree);
        assert_eq!(handle, rendered_icon(&world, second.nodes["root"].root, 24));
        update(
            &mut world,
            &tree,
            "root",
            &tree.nodes["root"],
            None,
            &first.nodes["root"],
        );
        assert_eq!(handle, rendered_icon(&world, first.nodes["root"].root, 24));
        assert_eq!(world.resource::<Assets<Image>>().len(), 1);
    }

    #[test]
    fn template_image_src_substitutes_cells() {
        let dir = tempfile::tempdir().unwrap();
        let paths = [dir.path().join("one.svg"), dir.path().join("two.svg")];
        for path in &paths {
            std::fs::write(path, ICON_SVG).unwrap();
        }
        let source = format!(
            "---\nscene: 1\nname: icons\ncitizen: test\n---\n```mix\nroot: {{widget: \"list\", row: \"icon\", row_height: 24, rows: {}}}\nicon: {{widget: \"image\", src: \"{{cells[1]}}\", w: 24, h: 24}}\n```\n",
            json!([{"id":"one","cells":["One",paths[0]]},{"id":"two","cells":["Two",paths[1]]}])
        );
        let tree = cosmix_scene::resolve(&cosmix_scene::parse(&source).unwrap()).unwrap();
        let mut world = World::new();
        let mut mounted = mounted(&mut world, &tree);
        apply(&mut world, &mut mounted, &tree);
        let model = ListModel(mounted.nodes["root"].list.as_ref().unwrap().clone());
        for (index, path) in paths.iter().enumerate() {
            let content = world.spawn_empty().id();
            model.bind(&mut world, content, index);
            let entity = world.get::<Children>(content).unwrap()[0];
            let handle = rendered_icon(&world, entity, 24);
            assert_eq!(
                Some(handle),
                icons::load(&mut world, path.to_str().unwrap(), 24.0, 24.0)
            );
        }
        assert_eq!(world.resource::<Assets<Image>>().len(), 2);
    }

    #[test]
    fn invalid_icons_clear_previous_images_and_are_cached() {
        let dir = tempfile::tempdir().unwrap();
        let valid = dir.path().join("valid.svg");
        std::fs::write(&valid, ICON_SVG).unwrap();
        let valid_tree = icon_tree(&valid);
        for (name, contents, size) in [
            ("broken.svg", "not SVG", 24.0),
            ("zero.svg", ICON_SVG, 0.0),
            ("large-target.svg", ICON_SVG, 1025.0),
            ("large-file.svg", ICON_SVG, 24.0),
        ] {
            let path = dir.path().join(name);
            std::fs::write(&path, contents).unwrap();
            if name == "large-file.svg" {
                std::fs::OpenOptions::new()
                    .write(true)
                    .open(&path)
                    .unwrap()
                    .set_len(4 * 1024 * 1024 + 1)
                    .unwrap();
            }
            let mut tree = icon_tree(&path);
            tree.nodes["root"].ports.insert("w".into(), json!(size));
            let mut world = World::new();
            let view = spawn(&mut world, &tree, "root", &tree.nodes["root"]);
            update(
                &mut world,
                &valid_tree,
                "root",
                &valid_tree.nodes["root"],
                None,
                &view,
            );
            rendered_icon(&world, view.root, 24);
            update(&mut world, &tree, "root", &tree.nodes["root"], None, &view);
            assert!(world.get::<ImageNode>(view.root).is_none(), "{name}");
            assert!(icons::load(&mut world, path.to_str().unwrap(), size, 24.0).is_none());
            if size == 0.0 || size > 1024.0 {
                assert!(icons::load(&mut world, path.to_str().unwrap(), 24.0, 24.0).is_some());
            }
            std::fs::write(&path, ICON_SVG).unwrap();
            assert!(icons::load(&mut world, path.to_str().unwrap(), 24.0, 24.0).is_some());
        }
    }

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
    fn unchanged_text_is_not_reinserted_when_a_sibling_changes() {
        // A panel re-renders every minute (the clock) and on every window
        // event; re-inserting unchanged Text components re-lays them out and
        // blanks every label for a frame. Only the text that changed may be
        // touched.
        let doc = |clock: &str| {
            format!(
                "---\nscene: 1\nname: panel\ncitizen: test\n---\n```mix\nroot: {{widget: \"row\", align: \"center\", children: [\"label\", \"clock\"]}}\nlabel: {{widget: \"text\", text: \"foot\"}}\nclock: {{widget: \"text\", text: \"{clock}\"}}\n```\n"
            )
        };
        let first = cosmix_scene::resolve(&cosmix_scene::parse(&doc("09:05 pm")).unwrap()).unwrap();
        let second =
            cosmix_scene::resolve(&cosmix_scene::parse(&doc("09:06 pm")).unwrap()).unwrap();
        let mut world = World::new();
        let mut mounted = mounted(&mut world, &first);
        apply(&mut world, &mut mounted, &first);
        let label = mounted.nodes["label"].label.unwrap();
        let clock = mounted.nodes["clock"].label.unwrap();
        let label_root = mounted.nodes["label"].root;
        let label_tick = world
            .entity(label)
            .get_ref::<Text>()
            .unwrap()
            .last_changed();
        let parent_tick = world
            .entity(label_root)
            .get_ref::<ChildOf>()
            .unwrap()
            .last_changed();
        world.increment_change_tick();
        apply(&mut world, &mut mounted, &second);
        assert_eq!(
            world
                .entity(label)
                .get_ref::<Text>()
                .unwrap()
                .last_changed(),
            label_tick,
            "the unchanged label must not be re-inserted"
        );
        // No node added, removed or re-childed: the hierarchy is left alone
        // (detach + re-parent re-lays out every text for a few frames).
        assert_eq!(
            world
                .entity(label_root)
                .get_ref::<ChildOf>()
                .unwrap()
                .last_changed(),
            parent_tick,
            "a non-structural revision must not re-parent scene roots"
        );
        assert_eq!(world.get::<Text>(clock).unwrap().0, "09:06 pm");
    }

    #[test]
    fn text_alignment_positions_an_intrinsic_label_without_wrapping() {
        for (family, align, expected) in [
            ("row", "center", AlignSelf::Auto),
            ("column", "center", AlignSelf::Auto),
            ("column", "stretch", AlignSelf::Stretch),
        ] {
            let source = format!(
                "---\nscene: 1\nname: alignment\ncitizen: test\n---\n```mix\nroot: {{widget: \"{family}\", align: \"{align}\", children: [\"label\"]}}\nlabel: {{widget: \"text\", text: \"Clock\", align: \"center\", fill: true}}\n```\n"
            );
            let mut tree = cosmix_scene::resolve(&cosmix_scene::parse(&source).unwrap()).unwrap();
            let mut world = World::new();
            let mut mounted = mounted(&mut world, &tree);
            apply(&mut world, &mut mounted, &tree);
            let view = &mounted.nodes["label"];
            assert_eq!(world.get::<Node>(view.root).unwrap().align_self, expected);
            assert_eq!(
                world.get::<Node>(view.label.unwrap()).unwrap().width,
                Val::Auto
            );
            assert_eq!(
                world
                    .get::<TextLayout>(view.label.unwrap())
                    .unwrap()
                    .justify,
                Justify::Center
            );
            tree.nodes["root"]
                .ports
                .insert("align".into(), json!("center"));
            apply(&mut world, &mut mounted, &tree);
            assert_eq!(
                world
                    .get::<Node>(mounted.nodes["label"].root)
                    .unwrap()
                    .align_self,
                AlignSelf::Auto
            );
        }
        for sizing in ["width: 120", "fill: true"] {
            let source = format!(
                "---\nscene: 1\nname: alignment\ncitizen: test\n---\n```mix\nroot: {{widget: \"text\", text: \"Clock\", align: \"center\", {sizing}}}\n```\n"
            );
            let mut tree = cosmix_scene::resolve(&cosmix_scene::parse(&source).unwrap()).unwrap();
            let mut world = World::new();
            let mut mounted = mounted(&mut world, &tree);
            apply(&mut world, &mut mounted, &tree);
            let label = mounted.nodes["root"].label.unwrap();
            assert_eq!(
                world.get::<TextLayout>(label).unwrap().justify,
                Justify::Center
            );
            assert_eq!(world.get::<Node>(label).unwrap().width, Val::Auto);
            assert_eq!(world.get::<Node>(label).unwrap().flex_shrink, 0.0);
            assert_eq!(
                world
                    .get::<Node>(mounted.nodes["root"].root)
                    .unwrap()
                    .justify_content,
                JustifyContent::Center
            );
            let root = world.get::<Node>(mounted.nodes["root"].root).unwrap();
            if sizing.starts_with("width") {
                assert_eq!(root.width, px(120));
            } else {
                assert_eq!(root.flex_grow, 1.0);
                assert_eq!(root.align_self, AlignSelf::Auto);
            }
            for (align, justify) in [("right", Justify::Right), ("left", Justify::Left)] {
                tree.nodes
                    .get_mut("root")
                    .unwrap()
                    .ports
                    .insert("align".into(), json!(align));
                apply(&mut world, &mut mounted, &tree);
                assert_eq!(world.get::<TextLayout>(label).unwrap().justify, justify);
            }
        }
    }

    #[test]
    fn column_align_center_sets_align_items() {
        let source = "---\nscene: 1\nname: alignment\ncitizen: test\n---\n```mix\nroot: {widget: \"column\", children: [], align: \"center\"}\n```\n";
        let mut tree = cosmix_scene::resolve(&cosmix_scene::parse(source).unwrap()).unwrap();
        let mut world = World::new();
        let mut mounted = mounted(&mut world, &tree);
        for (align, expected) in [
            ("center", AlignItems::Center),
            ("end", AlignItems::End),
            ("stretch", AlignItems::Stretch),
            ("start", AlignItems::Start),
        ] {
            tree.nodes
                .get_mut("root")
                .unwrap()
                .ports
                .insert("align".into(), json!(align));
            apply(&mut world, &mut mounted, &tree);
            assert_eq!(
                world
                    .get::<Node>(mounted.nodes["root"].root)
                    .unwrap()
                    .align_items,
                expected
            );
        }
    }

    #[test]
    fn chromeless_scene_hides_panel_header() {
        use cosmix_shell::chrome::{
            QuoinChromePlugin, QuoinContentBindings, QuoinPageRegistry, QuoinPanelMounts,
            mount_page, spawn_quoin_chrome,
        };
        use cosmix_shell::core::{LogicalSize, OutputKey, ShellModel};
        use cosmix_shell::runtime::{
            SceneVerb, ShellFrameState, ShellRuntimePlugin, set_shell_pages,
        };
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
            .add_plugins(ShellRuntimePlugin::new(model))
            .add_plugins(QuoinChromePlugin)
            .init_resource::<ButtonInput<KeyCode>>();
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
        let source = "---\nscene: 1\nname: chrome-test\ncitizen: test\nwindow: {\"kind\":\"edge\",\"edge\":\"bottom\",\"chrome\":false}\n---\n```mix\nroot: {widget: \"column\", children: []}\n```\n";
        let mut store = SceneStore::default();
        store
            .request(SceneVerb::Load, source, &Value::Null)
            .unwrap();
        world.insert_resource(store);
        reconcile(world);
        let page = world.resource::<SceneStore>().scenes["chrome-test"]
            .mounted
            .as_ref()
            .unwrap()
            .page;
        let wrapper = world.get::<ChildOf>(page).unwrap().parent();
        let host = world.get::<ChildOf>(wrapper).unwrap().parent();
        let panel = world.get::<ChildOf>(host).unwrap().parent();
        let header = world.get::<Children>(panel).unwrap()[0];
        app.update();
        assert_eq!(
            app.world().get::<Node>(header).unwrap().display,
            Display::None
        );
        let normal = app.world_mut().spawn(Node::default()).id();
        assert!(mount_page(
            app.world_mut(),
            Edge::Bottom,
            "normal",
            "Normal",
            normal
        ));
        // Mounting does not select the normal page; the user does.
        set_shell_pages(
            app.world_mut(),
            Edge::Bottom,
            vec!["scene-chrome-test".into(), "normal".into()],
            Some("normal"),
        );
        app.update();
        assert_eq!(
            app.world().get::<Node>(header).unwrap().display,
            Display::Flex
        );
        set_shell_pages(
            app.world_mut(),
            Edge::Bottom,
            vec!["scene-chrome-test".into(), "normal".into()],
            Some("scene-chrome-test"),
        );
        app.update();
        assert_eq!(
            app.world().get::<Node>(header).unwrap().display,
            Display::None
        );
        // Reload the envelope, then use a node-only window and patch its flag.
        app.world_mut()
            .resource_mut::<SceneStore>()
            .request(
                SceneVerb::Load,
                &source.replace("false", "true"),
                &Value::Null,
            )
            .unwrap();
        reconcile(app.world_mut());
        app.update();
        assert_eq!(
            app.world().get::<Node>(header).unwrap().display,
            Display::Flex
        );
        let node_source = "---\nscene: 1\nname: chrome-test\ncitizen: test\n---\n```mix\nroot: {widget: \"window\", kind: \"edge\", edge: \"bottom\", chrome: false}\n```\n";
        app.world_mut()
            .resource_mut::<SceneStore>()
            .request(SceneVerb::Load, node_source, &Value::Null)
            .unwrap();
        reconcile(app.world_mut());
        app.update();
        assert_eq!(
            app.world().get::<Node>(header).unwrap().display,
            Display::None
        );
        for value in [json!(true), json!(false), Value::Null, json!(false)] {
            app.world_mut()
                .resource_mut::<SceneStore>()
                .request(
                    SceneVerb::Patch,
                    "",
                    &json!({"scene":"chrome-test","path":"root.chrome","value":value}),
                )
                .unwrap();
            reconcile(app.world_mut());
            app.update();
            let expected = if value == json!(false) {
                Display::None
            } else {
                Display::Flex
            };
            assert_eq!(app.world().get::<Node>(header).unwrap().display, expected);
        }
        // Unloading the active chromeless page must reveal the remaining
        // normal page's header, not leave the panel permanently chromeless.
        app.world_mut()
            .resource_mut::<SceneStore>()
            .request(SceneVerb::Unload, "", &json!({"scene":"chrome-test"}))
            .unwrap();
        reconcile(app.world_mut());
        app.update();
        assert_eq!(
            app.world().get::<Node>(header).unwrap().display,
            Display::Flex
        );
        assert_eq!(
            app.world()
                .resource::<ShellFrameState>()
                .0
                .panel(Edge::Bottom)
                .active_page_id
                .as_deref(),
            Some("normal")
        );
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
    fn declared_panel_name_is_the_page_id() {
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
        // A declared sub-panel name in the window envelope is the mount
        // address; the scene's own name stays inside its grammar.
        store.request(
            SceneVerb::Load,
            "---\nscene: 1\nname: settings\ncitizen: test\nwindow: {\"kind\":\"edge\",\"edge\":\"right\",\"panel\":\"settings.appearance\"}\n---\n```mix\nroot: {widget: \"column\", children: []}\n```\n",
            &Value::Null,
        )
        .unwrap();
        store.request(
            SceneVerb::Load,
            "---\nscene: 1\nname: plain\ncitizen: test\n---\n```mix\nroot: {widget: \"window\", kind: \"edge\", edge: \"right\"}\n```\n",
            &Value::Null,
        )
        .unwrap();
        world.insert_resource(store);
        reconcile(world);
        let right = &world.resource::<ShellFrameState>().0.panel(Edge::Right).page_ids;
        // Reconciliation visits scenes in name order (BTreeMap), not load
        // order. The envelope overrides the page id, not that traversal.
        assert_eq!(right.as_ref(), &["scene-plain", "settings.appearance"]);
        // The mount survives a revision under the same declared name.
        world
            .resource_mut::<SceneStore>()
            .request(
                SceneVerb::Load,
                "---\nscene: 1\nname: settings\ncitizen: test\nwindow: {\"kind\":\"edge\",\"edge\":\"right\",\"panel\":\"settings.appearance\"}\n---\n```mix\nroot: {widget: \"column\", children: [\"extra\"]}\nextra: {widget: \"text\", text: \"x\"}\n```\n",
                &Value::Null,
            )
            .unwrap();
        reconcile(world);
        let right = &world.resource::<ShellFrameState>().0.panel(Edge::Right).page_ids;
        assert_eq!(right.as_ref(), &["scene-plain", "settings.appearance"]);
        // A live panel address cannot be aliased or renamed.
        for (name, panel) in [("impostor", "settings.appearance"), ("settings", "renamed")] {
            let result = world.resource_mut::<SceneStore>().request(
                SceneVerb::Load,
                &format!("---\nscene: 1\nname: {name}\ncitizen: test\nwindow: {{\"kind\":\"edge\",\"edge\":\"right\",\"panel\":\"{panel}\"}}\n---\n```mix\nroot: {{widget: \"column\", children: []}}\n```\n"),
                &Value::Null,
            );
            assert!(result.is_err(), "a live address cannot be aliased or renamed");
        }
        // An empty or non-string panel field falls back to the anonymous id.
        for window in ["{\"kind\":\"edge\",\"edge\":\"bottom\",\"panel\":\"\"}", "{\"kind\":\"edge\",\"edge\":\"bottom\",\"panel\":7}"] {
            let mut fallback = SceneStore::default();
            fallback
                .request(
                    SceneVerb::Load,
                    &format!(
                        "---\nscene: 1\nname: fb\ncitizen: test\nwindow: {window}\n---\n```mix\nroot: {{widget: \"column\", children: []}}\n```\n"
                    ),
                    &Value::Null,
                )
                .unwrap();
            assert_eq!(page_id(&fallback.scenes["fb"].tree), "scene-fb");
        }
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
        // Clearing an authored extent cannot reset the edge's seeded size.
        assert_eq!(frame.panel(Edge::Right).thickness_px, 200.0);
        assert!(!frame.panel(Edge::Right).mapped);
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

    /// Exercise the reservation used by production ingress and actual chrome.
    #[test]
    fn scene_mounts_feed_the_subpanel_owner_map() {
        use cosmix_shell::chrome::{
            QuoinChromePlugin, QuoinContentBindings, QuoinPageRegistry, QuoinPanelMounts,
            spawn_quoin_chrome,
        };
        use cosmix_shell::core::{LogicalSize, OutputKey, ShellModel};
        use cosmix_shell::runtime::{
            SceneVerb, ShellFrameState, ShellRuntimePlugin, SubPanelRegistryState,
        };
        fn request(
            world: &mut World,
            verb: SceneVerb,
            body: &str,
            args: &Value,
            owner: &str,
            receipt: u64,
        ) -> Result<(Value, Option<Value>), Value> {
            world.resource_scope(|world, mut store: Mut<SceneStore>| {
                world.resource_scope(|world, mut registry: Mut<SubPanelRegistryState>| {
                    store.request_mounted(
                        verb,
                        body,
                        args,
                        Some(&mut super::super::SceneMount {
                            registry: &mut registry.0,
                            output: &world.resource::<ShellFrameState>().0.geometry.output,
                            owner,
                            accepted_at: receipt,
                        }),
                    )
                })
            })
        }
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
            .add_plugins(ShellRuntimePlugin::new(model))
            .add_plugins(QuoinChromePlugin)
            .init_resource::<SceneStore>();
        let world = app.world_mut();
        let props = QuoinPageRegistry::new(vec![], vec![], vec![], vec![])
            .unwrap()
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
        spawn_quoin_chrome(&mut world.commands(), mounts, props);
        world.flush();
        let source = |citizen: &str| {
            format!(
                "---\nscene: 1\nname: feed-test\ncitizen: {citizen}\n---\n```mix\nroot: {{widget: \"window\", kind: \"edge\", edge: \"left\", w: 200}}\n```\n"
            )
        };
        request(
            world,
            SceneVerb::Load,
            &source("metadata"),
            &Value::Null,
            "sender",
            1,
        )
        .unwrap();
        reconcile(world);
        let page = world.resource::<SceneStore>().scenes["feed-test"]
            .mounted
            .as_ref()
            .unwrap()
            .page;
        let seat = world
            .resource::<SubPanelRegistryState>()
            .0
            .seat("scene-feed-test")
            .unwrap();
        assert_eq!(
            (seat.edge, seat.owner.as_str(), seat.accepted_at),
            (Edge::Left, "sender", 1)
        );

        // Authored citizen revisions cannot transfer lifecycle ownership.
        request(
            world,
            SceneVerb::Load,
            &source("other-metadata"),
            &Value::Null,
            "sender",
            2,
        )
        .unwrap();
        reconcile(world);
        assert_eq!(
            world
                .resource::<SubPanelRegistryState>()
                .0
                .seat("scene-feed-test")
                .unwrap()
                .owner,
            "sender"
        );
        assert_eq!(
            world.resource::<SceneStore>().scenes["feed-test"]
                .mounted
                .as_ref()
                .unwrap()
                .page,
            page
        );
        assert!(
            request(
                world,
                SceneVerb::Load,
                &source("metadata"),
                &Value::Null,
                "other-sender",
                3
            )
            .is_err()
        );
        assert!(
            request(
                world,
                SceneVerb::Patch,
                "",
                &json!({
                    "scene":"feed-test", "path":"root.edge", "value":"right"
                }),
                "sender",
                3
            )
            .is_err()
        );

        // Old teardown must not release a replacement accepted in the same frame.
        request(
            world,
            SceneVerb::Unload,
            "",
            &json!({"scene":"feed-test"}),
            "sender",
            3,
        )
        .unwrap();
        request(
            world,
            SceneVerb::Load,
            &source("metadata"),
            &Value::Null,
            "replacement",
            3,
        )
        .unwrap();
        reconcile(world);
        assert!(world.get_entity(page).is_err());
        assert_eq!(
            world
                .resource::<SubPanelRegistryState>()
                .0
                .seat("scene-feed-test")
                .unwrap()
                .owner,
            "replacement"
        );
        assert!(
            world.resource::<SceneStore>().scenes["feed-test"]
                .mounted
                .as_ref()
                .unwrap()
                .registered
        );
        request(
            world,
            SceneVerb::Unload,
            "",
            &json!({"scene":"feed-test"}),
            "replacement",
            4,
        )
        .unwrap();
        reconcile(world);
        assert!(
            world
                .resource::<SubPanelRegistryState>()
                .0
                .seat("scene-feed-test")
                .is_none()
        );
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
