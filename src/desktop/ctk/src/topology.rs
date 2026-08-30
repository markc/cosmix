//! Reusable pan/zoom topology canvas for node-and-edge desktop surfaces.
//!
//! The canvas owns interaction and geometry, while applications own the graph
//! model. Nodes are keyboard-focusable buttons, edges name their endpoint node
//! ids, and the plugin keeps both layers under one zoom-limited viewport transform.

use std::collections::HashMap;

use accesskit::Role;
use bevy::a11y::AccessibilityNode;
use bevy::app::{App, Plugin, Update};
use bevy::ecs::entity::Entity;
use bevy::ecs::observer::On;
use bevy::ecs::system::{Commands, Local, Query};
use bevy::feathers::theme::{ThemeBackgroundColor, ThemeTextColor, UiTheme};
use bevy::input::keyboard::{KeyCode, KeyboardInput};
use bevy::input::ButtonState;
use bevy::input_focus::tab_navigation::TabIndex;
use bevy::input_focus::FocusedInput;
use bevy::math::{Rot2, Vec2};
use bevy::picking::events::{Drag, Pointer, Scroll};
use bevy::picking::Pickable;
use bevy::prelude::{
    default, AlignItems, BorderColor, BorderRadius, Button, Changed, Color, Component,
    DetectChanges, Display, FlexDirection, JustifyContent, Node, Ref, RemovedComponents, Text,
    TextFont, UiTransform,
};
use bevy::ui::{percent, px, UiRect, Val2};
use bevy::ui_widgets::Activate;

use crate::theme::tokens;

const DEFAULT_NODE_SIZE: Vec2 = Vec2::new(156.0, 76.0);

/// Reusable topology interaction systems.
pub struct TopologyCanvasPlugin;

impl Plugin for TopologyCanvasPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            Update,
            (
                sync_canvas_transform,
                sync_node_geometry,
                sync_edge_geometry,
                sync_selected_node,
            ),
        );
    }
}

/// Canvas zoom bounds and keyboard increments.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TopologyCanvasProps {
    pub min_zoom: f32,
    pub max_zoom: f32,
    pub zoom_step: f32,
    pub keyboard_pan_step: f32,
}

impl Default for TopologyCanvasProps {
    fn default() -> Self {
        Self {
            min_zoom: 0.45,
            max_zoom: 2.5,
            zoom_step: 0.12,
            keyboard_pan_step: 32.0,
        }
    }
}

/// Mutable viewport and selection state stored on the canvas root.
#[derive(Component, Clone, Debug, PartialEq)]
pub struct TopologyCanvasState {
    pan: Vec2,
    zoom: f32,
    selected: Option<String>,
    props: TopologyCanvasProps,
}

impl TopologyCanvasState {
    fn new(props: TopologyCanvasProps) -> Self {
        let mut state = Self {
            pan: Vec2::ZERO,
            zoom: 1.0,
            selected: None,
            props,
        };
        state.zoom = state.clamp_zoom(state.zoom);
        state
    }

    pub fn pan(&self) -> Vec2 {
        self.pan
    }

    pub fn zoom(&self) -> f32 {
        self.zoom
    }

    pub fn selected(&self) -> Option<&str> {
        self.selected.as_deref()
    }

    pub fn pan_by(&mut self, delta: Vec2) {
        if delta.is_finite() {
            self.pan += delta;
        }
    }

    pub fn zoom_by(&mut self, delta: f32) {
        if delta.is_finite() {
            self.zoom = self.clamp_zoom(self.zoom + delta);
        }
    }

    pub fn select(&mut self, id: impl Into<String>) {
        self.selected = Some(id.into());
    }

    pub fn clear_selection(&mut self) {
        self.selected = None;
    }

    pub fn reset_view(&mut self) {
        self.pan = Vec2::ZERO;
        self.zoom = self.clamp_zoom(1.0);
    }

    fn clamp_zoom(&self, zoom: f32) -> f32 {
        let min = self.props.min_zoom.max(0.05);
        let max = self.props.max_zoom.max(min);
        zoom.clamp(min, max)
    }
}

/// Layer ownership stored on the canvas root.
#[derive(Component, Clone, Copy, Debug)]
pub struct TopologyCanvas {
    pub content: Entity,
    pub edge_layer: Entity,
    pub node_layer: Entity,
}

/// Entities returned by [`spawn_topology_canvas`].
#[derive(Clone, Copy, Debug)]
pub struct TopologyCanvasEntities {
    pub root: Entity,
    pub content: Entity,
    pub edge_layer: Entity,
    pub node_layer: Entity,
}

/// Spawn an accessible canvas with distinct edge and node layers.
pub fn spawn_topology_canvas(
    commands: &mut Commands,
    props: TopologyCanvasProps,
) -> TopologyCanvasEntities {
    let edge_layer = commands
        .spawn((
            Node {
                position_type: bevy::ui::PositionType::Absolute,
                width: percent(100),
                height: percent(100),
                ..default()
            },
            Pickable::IGNORE,
        ))
        .id();
    let node_layer = commands
        .spawn((
            Node {
                position_type: bevy::ui::PositionType::Absolute,
                width: percent(100),
                height: percent(100),
                ..default()
            },
            Pickable::IGNORE,
        ))
        .id();
    let content = commands
        .spawn((
            Node {
                position_type: bevy::ui::PositionType::Absolute,
                width: percent(100),
                height: percent(100),
                ..default()
            },
            UiTransform::default(),
            Pickable::IGNORE,
        ))
        .add_children(&[edge_layer, node_layer])
        .id();

    let mut accessibility = accesskit::Node::new(Role::Group);
    accessibility.set_label("Topology canvas; arrow keys pan, plus and minus zoom, Home resets");
    let root = commands
        .spawn((
            Node {
                width: percent(100),
                height: percent(100),
                min_width: px(0),
                min_height: px(0),
                position_type: bevy::ui::PositionType::Relative,
                overflow: bevy::ui::Overflow::clip(),
                ..default()
            },
            ThemeBackgroundColor(tokens::TRACK),
            Pickable::default(),
            TabIndex(0),
            AccessibilityNode::from(accessibility),
            TopologyCanvasState::new(props),
            TopologyCanvas {
                content,
                edge_layer,
                node_layer,
            },
        ))
        .add_child(content)
        .observe(on_canvas_drag)
        .observe(on_canvas_scroll)
        .observe(on_canvas_key)
        .id();

    TopologyCanvasEntities {
        root,
        content,
        edge_layer,
        node_layer,
    }
}

/// Application-supplied node identity, label and canvas-space geometry.
#[derive(Clone, Debug)]
pub struct TopologyNodeProps {
    pub id: String,
    pub label: String,
    pub position: Vec2,
    pub size: Vec2,
}

impl TopologyNodeProps {
    pub fn new(id: impl Into<String>, label: impl Into<String>, position: Vec2) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            position,
            size: DEFAULT_NODE_SIZE,
        }
    }
}

/// One selectable topology entity.
#[derive(Component, Clone, Debug, PartialEq)]
pub struct TopologyNode {
    pub canvas: Entity,
    pub id: String,
    pub position: Vec2,
    pub size: Vec2,
}

/// Entities returned by [`spawn_topology_node`].
#[derive(Clone, Copy, Debug)]
pub struct TopologyNodeEntities {
    pub root: Entity,
    pub label: Entity,
}

/// Spawn a keyboard-focusable node into a canvas node layer.
pub fn spawn_topology_node(
    commands: &mut Commands,
    canvas: Entity,
    node_layer: Entity,
    props: TopologyNodeProps,
) -> TopologyNodeEntities {
    let label = commands
        .spawn((
            Text::new(props.label),
            TextFont::from_font_size(13.0),
            ThemeTextColor(tokens::TEXT),
            Pickable::IGNORE,
        ))
        .id();
    let mut accessibility = accesskit::Node::new(Role::Button);
    accessibility.set_label(format!("Select topology node {}", props.id));
    let root = commands
        .spawn((
            Button,
            Node {
                position_type: bevy::ui::PositionType::Absolute,
                width: px(props.size.x),
                height: px(props.size.y),
                padding: UiRect::all(px(8)),
                border: UiRect::all(px(2)),
                border_radius: BorderRadius::all(px(7)),
                flex_direction: FlexDirection::Column,
                align_items: AlignItems::Center,
                justify_content: JustifyContent::Center,
                ..default()
            },
            ThemeBackgroundColor(tokens::PANEL),
            BorderColor::all(Color::NONE),
            TabIndex(0),
            AccessibilityNode::from(accessibility),
            TopologyNode {
                canvas,
                id: props.id,
                position: props.position,
                size: props.size,
            },
        ))
        .add_child(label)
        .observe(on_node_activate)
        .id();
    commands.entity(node_layer).add_child(root);
    TopologyNodeEntities { root, label }
}

/// Application-supplied edge endpoints.
#[derive(Clone, Debug)]
pub struct TopologyEdgeProps {
    pub from: String,
    pub to: String,
}

impl TopologyEdgeProps {
    pub fn new(from: impl Into<String>, to: impl Into<String>) -> Self {
        Self {
            from: from.into(),
            to: to.into(),
        }
    }
}

/// One edge whose geometry is derived from two [`TopologyNode`] ids.
#[derive(Component, Clone, Debug, PartialEq, Eq)]
pub struct TopologyEdge {
    pub canvas: Entity,
    pub from: String,
    pub to: String,
}

/// Spawn an edge into the canvas edge layer.
pub fn spawn_topology_edge(
    commands: &mut Commands,
    canvas: Entity,
    edge_layer: Entity,
    props: TopologyEdgeProps,
) -> Entity {
    let edge = commands
        .spawn((
            Node {
                position_type: bevy::ui::PositionType::Absolute,
                height: px(2),
                ..default()
            },
            ThemeBackgroundColor(tokens::BORDER),
            UiTransform::default(),
            Pickable::IGNORE,
            TopologyEdge {
                canvas,
                from: props.from,
                to: props.to,
            },
        ))
        .id();
    commands.entity(edge_layer).add_child(edge);
    edge
}

fn on_canvas_drag(mut drag: On<Pointer<Drag>>, mut canvases: Query<&mut TopologyCanvasState>) {
    if drag.original_event_target() != drag.entity {
        return;
    }
    let Ok(mut state) = canvases.get_mut(drag.entity) else {
        return;
    };
    drag.propagate(false);
    state.pan_by(drag.delta);
}

fn on_canvas_scroll(
    mut scroll: On<Pointer<Scroll>>,
    mut canvases: Query<&mut TopologyCanvasState>,
) {
    let Ok(mut state) = canvases.get_mut(scroll.entity) else {
        return;
    };
    scroll.propagate(false);
    let step = state.props.zoom_step;
    state.zoom_by(scroll.y.signum() * step);
}

fn on_canvas_key(
    mut input: On<FocusedInput<KeyboardInput>>,
    mut canvases: Query<&mut TopologyCanvasState>,
) {
    if input.input.state != ButtonState::Pressed {
        return;
    }
    let Ok(mut state) = canvases.get_mut(input.focused_entity) else {
        return;
    };
    let pan = state.props.keyboard_pan_step;
    let zoom = state.props.zoom_step;
    let handled = match input.input.key_code {
        KeyCode::ArrowLeft => {
            state.pan_by(Vec2::new(pan, 0.0));
            true
        }
        KeyCode::ArrowRight => {
            state.pan_by(Vec2::new(-pan, 0.0));
            true
        }
        KeyCode::ArrowUp => {
            state.pan_by(Vec2::new(0.0, pan));
            true
        }
        KeyCode::ArrowDown => {
            state.pan_by(Vec2::new(0.0, -pan));
            true
        }
        KeyCode::Equal | KeyCode::NumpadAdd => {
            state.zoom_by(zoom);
            true
        }
        KeyCode::Minus | KeyCode::NumpadSubtract => {
            state.zoom_by(-zoom);
            true
        }
        KeyCode::Home => {
            state.reset_view();
            true
        }
        _ => false,
    };
    if handled {
        input.propagate(false);
    }
}

fn on_node_activate(
    activate: On<Activate>,
    nodes: Query<&TopologyNode>,
    mut canvases: Query<&mut TopologyCanvasState>,
) {
    let Ok(node) = nodes.get(activate.entity) else {
        return;
    };
    if let Ok(mut canvas) = canvases.get_mut(node.canvas) {
        canvas.select(node.id.clone());
    }
}

fn sync_canvas_transform(
    canvases: Query<(&TopologyCanvasState, &TopologyCanvas), Changed<TopologyCanvasState>>,
    mut transforms: Query<&mut UiTransform>,
) {
    for (state, canvas) in &canvases {
        let Ok(mut transform) = transforms.get_mut(canvas.content) else {
            continue;
        };
        transform.translation = Val2::new(px(state.pan.x), px(state.pan.y));
        transform.scale = Vec2::splat(state.zoom);
    }
}

fn sync_node_geometry(mut nodes: Query<(&TopologyNode, &mut Node), Changed<TopologyNode>>) {
    for (topology, mut node) in &mut nodes {
        node.left = px(topology.position.x);
        node.top = px(topology.position.y);
        node.width = px(topology.size.x);
        node.height = px(topology.size.y);
    }
}

fn sync_edge_geometry(
    nodes: Query<Ref<TopologyNode>>,
    mut removed_nodes: RemovedComponents<TopologyNode>,
    mut edges: Query<(Ref<TopologyEdge>, &mut Node, &mut UiTransform)>,
    mut positions: Local<HashMap<(Entity, String), (Vec2, Vec2)>>,
    mut cache_primed: Local<bool>,
) {
    // A dedicated primed flag: a legitimately EMPTY topology must not read as
    // "cache never built" and re-dirty every frame.
    let nodes_changed = !*cache_primed
        || removed_nodes.read().next().is_some()
        || nodes.iter().any(|node| node.is_changed());
    if nodes_changed {
        *cache_primed = true;
        positions.clear();
        for node in &nodes {
            positions.insert((node.canvas, node.id.clone()), (node.position, node.size));
        }
    }
    for (edge, mut node, mut transform) in &mut edges {
        if !nodes_changed && !edge.is_changed() {
            continue;
        }
        let Some((from, from_size)) = positions.get(&(edge.canvas, edge.from.clone())) else {
            if node.display != Display::None {
                node.display = Display::None;
            }
            continue;
        };
        let Some((to, to_size)) = positions.get(&(edge.canvas, edge.to.clone())) else {
            if node.display != Display::None {
                node.display = Display::None;
            }
            continue;
        };
        let start = *from + *from_size * 0.5;
        let end = *to + *to_size * 0.5;
        let geometry = edge_geometry(start, end);
        if node.display != Display::Flex {
            node.display = Display::Flex;
        }
        let left = px(geometry.left);
        let top = px(geometry.top);
        let width = px(geometry.length);
        if node.left != left {
            node.left = left;
        }
        if node.top != top {
            node.top = top;
        }
        if node.width != width {
            node.width = width;
        }
        let rotation = Rot2::radians(geometry.angle);
        if transform.rotation != rotation {
            transform.rotation = rotation;
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct EdgeGeometry {
    left: f32,
    top: f32,
    length: f32,
    angle: f32,
}

fn edge_geometry(start: Vec2, end: Vec2) -> EdgeGeometry {
    let delta = end - start;
    let length = delta.length();
    let midpoint = (start + end) * 0.5;
    EdgeGeometry {
        // UiTransform rotates around the element centre, so place the
        // unrotated edge's centre on the endpoint midpoint.
        left: midpoint.x - length * 0.5,
        top: midpoint.y - 1.0,
        length,
        angle: delta.y.atan2(delta.x),
    }
}

fn sync_selected_node(
    theme: Option<bevy::prelude::Res<UiTheme>>,
    canvases: Query<&TopologyCanvasState>,
    mut nodes: Query<(&TopologyNode, &mut BorderColor)>,
) {
    for (node, mut border) in &mut nodes {
        let selected = canvases
            .get(node.canvas)
            .ok()
            .and_then(TopologyCanvasState::selected)
            == Some(node.id.as_str());
        let desired = if let Some(theme) = &theme {
            let token = if selected {
                tokens::CONTROL_ACTIVE
            } else {
                tokens::BORDER
            };
            theme.color(&token)
        } else if selected {
            Color::WHITE
        } else {
            Color::BLACK
        };
        let desired = BorderColor::all(desired);
        if *border != desired {
            *border = desired;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::app::App;

    #[test]
    fn viewport_clamps_zoom_and_ignores_non_finite_pan() {
        let mut state = TopologyCanvasState::new(TopologyCanvasProps::default());
        state.zoom_by(99.0);
        assert_eq!(state.zoom(), 2.5);
        state.zoom_by(-99.0);
        assert_eq!(state.zoom(), 0.45);
        state.pan_by(Vec2::new(f32::NAN, 2.0));
        assert_eq!(state.pan(), Vec2::ZERO);
        state.pan_by(Vec2::new(4.0, -3.0));
        assert_eq!(state.pan(), Vec2::new(4.0, -3.0));
    }

    #[test]
    fn plugin_resolves_horizontal_and_angled_edge_geometry() {
        let mut app = App::new();
        app.add_plugins(TopologyCanvasPlugin);
        let (horizontal, angled) = {
            let mut commands = app.world_mut().commands();
            let canvas = spawn_topology_canvas(&mut commands, TopologyCanvasProps::default());
            spawn_topology_node(
                &mut commands,
                canvas.root,
                canvas.node_layer,
                TopologyNodeProps::new("alpha", "alpha", Vec2::new(10.0, 20.0)),
            );
            spawn_topology_node(
                &mut commands,
                canvas.root,
                canvas.node_layer,
                TopologyNodeProps::new("beta", "beta", Vec2::new(210.0, 20.0)),
            );
            spawn_topology_node(
                &mut commands,
                canvas.root,
                canvas.node_layer,
                TopologyNodeProps::new("gamma", "gamma", Vec2::new(210.0, 120.0)),
            );
            let horizontal = spawn_topology_edge(
                &mut commands,
                canvas.root,
                canvas.edge_layer,
                TopologyEdgeProps::new("alpha", "beta"),
            );
            let angled = spawn_topology_edge(
                &mut commands,
                canvas.root,
                canvas.edge_layer,
                TopologyEdgeProps::new("alpha", "gamma"),
            );
            (horizontal, angled)
        };
        app.update();

        let horizontal_node = app.world().get::<Node>(horizontal).unwrap();
        assert_eq!(horizontal_node.display, Display::Flex);
        assert_eq!(horizontal_node.left, px(88.0));
        assert_eq!(horizontal_node.top, px(57.0));
        assert_eq!(horizontal_node.width, px(200.0));

        let angled_node = app.world().get::<Node>(angled).unwrap();
        let geometry = edge_geometry(Vec2::new(88.0, 58.0), Vec2::new(288.0, 158.0));
        assert_eq!(angled_node.left, px(geometry.left));
        assert_eq!(angled_node.top, px(geometry.top));
        assert_eq!(angled_node.width, px(geometry.length));
        assert_eq!(
            app.world().get::<UiTransform>(angled).unwrap().rotation,
            Rot2::radians(geometry.angle)
        );
    }

    #[test]
    fn node_selection_updates_shared_canvas_state() {
        let mut app = App::new();
        app.add_plugins(TopologyCanvasPlugin);
        let (canvas, node) = {
            let mut commands = app.world_mut().commands();
            let canvas = spawn_topology_canvas(&mut commands, TopologyCanvasProps::default());
            let node = spawn_topology_node(
                &mut commands,
                canvas.root,
                canvas.node_layer,
                TopologyNodeProps::new("alpha", "alpha", Vec2::ZERO),
            );
            (canvas, node)
        };
        app.update();
        app.world_mut().trigger(Activate { entity: node.root });
        app.update();
        assert_eq!(
            app.world()
                .get::<TopologyCanvasState>(canvas.root)
                .unwrap()
                .selected(),
            Some("alpha")
        );
    }
}
