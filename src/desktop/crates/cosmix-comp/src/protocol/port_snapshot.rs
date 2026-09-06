//! Owned, post-transaction snapshot and the P-0 property read schema.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering},
    },
};

use cosmix_props_core::PropPath;
use serde::Serialize;
use serde_json::{Value, json};
use smithay::{
    output::Output,
    wayland::shell::wlr_layer::{ExclusiveZone, KeyboardInteractivity, Layer as WlrLayer},
};

use super::{
    ChromePointerGrabKind, InteractivePointer, LayerOutputBinding, LockLifecycle,
    LogicalOutputRect, SceneDecorationMode, StackBand, SurfaceId, SurfaceRecord, SurfaceRole,
    WaylandState, corner::CornerConfig, surface_stack_cmp,
};

pub(crate) const BROKER_RETRYING: u8 = 0;
pub(crate) const BROKER_CONNECTED: u8 = 1;

/// Effective Bus message ceiling on the broker WebSocket path. The client
/// writes each Bus message as one frame, so both transport caps apply.
pub(crate) const MAX_REPLY_WIRE_BYTES: usize =
    if cosmix_bus::bus::MAX_MESSAGE_BYTES < cosmix_bus::bus::WS_MAX_FRAME_BYTES {
        cosmix_bus::bus::MAX_MESSAGE_BYTES
    } else {
        cosmix_bus::bus::WS_MAX_FRAME_BYTES
    };

/// Upper bound reserved inside [`MAX_REPLY_WIRE_BYTES`] for canonical Bus
/// framing and response headers (`command`, `from`, `to`, `type`, `rc`, and
/// broker correlation `id`). The reply sender also measures those actual bytes
/// immediately before sending; the corresponding test proves that maximal
/// grammar-valid service names and correlation headers stay within this bound.
pub(crate) const REPLY_WIRE_HEADROOM_BYTES: usize = 4 * 1024;
pub(crate) const MAX_REPLY_BODY_BYTES: usize = MAX_REPLY_WIRE_BYTES - REPLY_WIRE_HEADROOM_BYTES;

pub(super) fn exact_i32_to_f32(value: i32) -> Option<f32> {
    let converted = value as f32;
    (f64::from(converted) == f64::from(value)).then_some(converted)
}

pub(super) fn exact_logical_output_rect(
    x: i32,
    y: i32,
    width: i32,
    height: i32,
) -> Option<LogicalOutputRect> {
    Some(LogicalOutputRect {
        x: exact_i32_to_f32(x)?,
        y: exact_i32_to_f32(y)?,
        width: exact_i32_to_f32(width)?,
        height: exact_i32_to_f32(height)?,
    })
}

#[derive(Debug)]
pub(crate) struct SnapshotContext {
    pub(crate) service: Arc<str>,
    pub(crate) version: Arc<str>,
    pub(crate) backend: &'static str,
    pub(crate) engine: &'static str,
    pub(crate) instance: Arc<str>,
    pub(crate) decoration_enabled: bool,
    pub(crate) decoration_style: &'static str,
    pub(crate) broker: Arc<AtomicU8>,
    pub(crate) queue_depth: Arc<AtomicUsize>,
    pub(crate) reply_timeouts: Arc<AtomicU64>,
    pub(crate) publish_timeouts: Arc<AtomicU64>,
    pub(crate) event_seq: Arc<AtomicU64>,
    pub(crate) lost_count: Arc<AtomicU64>,
    pub(crate) pending_idle_order: Arc<AtomicU64>,
    pub(crate) pending_active_order: Arc<AtomicU64>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct CompSnapshot {
    pub(crate) info: InfoSnapshot,
    pub(crate) outputs: BTreeMap<String, OutputSnapshot>,
    pub(crate) surfaces: BTreeMap<String, SurfaceSnapshot>,
    pub(crate) windows: BTreeMap<String, WindowSnapshot>,
    pub(crate) stack: Vec<u64>,
    pub(crate) focus: FocusSnapshot,
    pub(crate) decoration: DecorationSnapshot,
    pub(crate) bindings: BindingsSnapshot,
    pub(crate) input: InputSnapshot,
    #[cfg(feature = "xwayland")]
    pub(crate) xwayland: XwaylandSnapshot,
    pub(crate) port: PortSnapshot,
    #[serde(skip)]
    full_tree: tokio::sync::OnceCell<SerialisedReply>,
}

#[derive(Clone, Debug)]
struct SerialisedReply {
    body: Arc<str>,
    bytes: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct InfoSnapshot {
    pub(crate) service: Arc<str>,
    pub(crate) version: Arc<str>,
    pub(crate) backend: &'static str,
    pub(crate) engine: &'static str,
    pub(crate) instance: Arc<str>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct OutputSnapshot {
    pub(crate) name: String,
    pub(crate) default: bool,
    pub(crate) x: i32,
    pub(crate) y: i32,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) scale: f64,
    pub(crate) refresh_mhz: u32,
    pub(crate) usable: RectSnapshot,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub(crate) struct RectSnapshot {
    pub(crate) x: f32,
    pub(crate) y: f32,
    pub(crate) width: f32,
    pub(crate) height: f32,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct SurfaceSnapshot {
    pub(crate) id: u64,
    pub(crate) role: &'static str,
    pub(crate) mapped: bool,
    pub(crate) visible: bool,
    pub(crate) x: f32,
    pub(crate) y: f32,
    pub(crate) width: f32,
    pub(crate) height: f32,
    pub(crate) band: &'static str,
    pub(crate) sequence: u64,
    pub(crate) tree_index: u32,
    pub(crate) parent: Option<u64>,
    pub(crate) output: Option<String>,
    pub(crate) title: Option<Arc<str>>,
    pub(crate) app_id: Option<Arc<str>>,
    pub(crate) focused: bool,
    pub(crate) activated: bool,
    pub(crate) maximized: bool,
    pub(crate) minimized: bool,
    pub(crate) decoration: Option<&'static str>,
    pub(crate) layer: Option<LayerSnapshot>,
    pub(crate) foreign_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct LayerSnapshot {
    pub(crate) stratum: &'static str,
    pub(crate) interactivity: &'static str,
    pub(crate) exclusive_zone: i32,
    pub(crate) binding: &'static str,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct WindowSnapshot {
    pub(crate) id: u64,
    pub(crate) foreign_id: Option<String>,
    pub(crate) title: Option<Arc<str>>,
    pub(crate) app_id: Option<Arc<str>>,
    pub(crate) x: f32,
    pub(crate) y: f32,
    pub(crate) width: f32,
    pub(crate) height: f32,
    pub(crate) focused: bool,
    pub(crate) maximized: bool,
    pub(crate) minimized: bool,
    pub(crate) output: Option<String>,
    pub(crate) band: &'static str,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct FocusSnapshot {
    pub(crate) keyboard: Option<u64>,
    pub(crate) exclusive_latch: Option<u64>,
    pub(crate) pointer: Option<u64>,
    pub(crate) pointer_grab: &'static str,
    pub(crate) session_lock: &'static str,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct DecorationSnapshot {
    pub(crate) enabled: bool,
    pub(crate) style: &'static str,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct BindingsSnapshot {
    pub(crate) enabled: bool,
    pub(crate) profile: &'static str,
    pub(crate) table: Vec<BindingRowSnapshot>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct BindingRowSnapshot {
    pub(crate) chord: String,
    pub(crate) action: &'static str,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct InputSnapshot {
    pub(crate) corners: CornersSnapshot,
}

/// The XWayland runtime switch as a props subtree: `xwayland.enabled` is
/// the CONFIGURED value (startup-read; a set persists for the next
/// compositor startup — not whether a generation is currently running,
/// which the lifecycle owns), and `xwayland.persist_path` is the resolved
/// per-socket file that value persists to — read-only, surfaced because
/// the path depends on the COSMIX root and the socket name, and an
/// operator must be able to SEE which file governs the next startup
/// rather than deduce it.
#[cfg(feature = "xwayland")]
#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct XwaylandSnapshot {
    pub(crate) enabled: bool,
    pub(crate) persist_path: Arc<str>,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub(crate) struct CornersSnapshot {
    pub(crate) enabled: bool,
    pub(crate) deadzone_px: f64,
    pub(crate) dwell_ms: u64,
    pub(crate) velocity_max_px_s: f64,
}

impl From<CornerConfig> for CornersSnapshot {
    fn from(config: CornerConfig) -> Self {
        Self {
            enabled: config.enabled,
            deadzone_px: config.deadzone_px,
            dwell_ms: config.dwell_ms,
            velocity_max_px_s: config.velocity_max_px_s,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct PortSnapshot {
    pub(crate) level: &'static str,
    pub(crate) event_seq: u64,
    pub(crate) lost_count: u64,
    pub(crate) queue_depth: usize,
    pub(crate) reply_timeouts: u64,
    pub(crate) publish_timeouts: u64,
    pub(crate) slug_collisions: u64,
    pub(crate) broker: &'static str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SnapshotNodeKind {
    Leaf,
    Object,
}

fn serialise_selected<T: Serialize>(value: &T) -> Option<Value> {
    serde_json::to_value(value).ok()
}

impl CompSnapshot {
    fn select(&self, path: &[&str]) -> Option<Value> {
        let [head, tail @ ..] = path else {
            return None;
        };
        match *head {
            "info" => self.info.select(tail),
            "outputs" => select_map(&self.outputs, tail, OutputSnapshot::select),
            "surfaces" => select_map(&self.surfaces, tail, SurfaceSnapshot::select),
            "windows" => select_map(&self.windows, tail, WindowSnapshot::select),
            "stack" if tail.is_empty() => serialise_selected(&self.stack),
            "focus" => self.focus.select(tail),
            "decoration" => self.decoration.select(tail),
            "bindings" => self.bindings.select(tail),
            "input" => self.input.select(tail),
            #[cfg(feature = "xwayland")]
            "xwayland" => self.xwayland.select(tail),
            "port" => self.port.select(tail),
            _ => None,
        }
    }

    fn node_kind(&self, path: &[&str]) -> Option<SnapshotNodeKind> {
        let [head, tail @ ..] = path else {
            return None;
        };
        match *head {
            "info" => self.info.node_kind(tail),
            "outputs" => map_node_kind(&self.outputs, tail, OutputSnapshot::node_kind),
            "surfaces" => map_node_kind(&self.surfaces, tail, SurfaceSnapshot::node_kind),
            "windows" => map_node_kind(&self.windows, tail, WindowSnapshot::node_kind),
            "stack" if tail.is_empty() => Some(SnapshotNodeKind::Leaf),
            "focus" => self.focus.node_kind(tail),
            "decoration" => self.decoration.node_kind(tail),
            "bindings" => self.bindings.node_kind(tail),
            "input" => self.input.node_kind(tail),
            #[cfg(feature = "xwayland")]
            "xwayland" => self.xwayland.node_kind(tail),
            "port" => self.port.node_kind(tail),
            _ => None,
        }
    }

    fn leaf_paths(&self) -> Vec<PropPath> {
        let mut paths = Vec::new();
        for descriptor in DESCRIPTORS {
            for candidate in self.expand_pattern(descriptor.pattern) {
                let Ok(path) = PropPath::new(candidate) else {
                    continue;
                };
                let segments = path.segments().collect::<Vec<_>>();
                if self.node_kind(&segments) == Some(SnapshotNodeKind::Leaf) {
                    paths.push(path);
                }
            }
        }
        paths.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        paths
    }

    fn expand_pattern(&self, pattern: &[PatternSegment]) -> Vec<String> {
        let mut paths = vec![String::new()];
        for segment in pattern {
            match segment {
                PatternSegment::Literal(segment) => append_segments(&mut paths, [*segment]),
                PatternSegment::OutputKey => {
                    append_segments(&mut paths, self.outputs.keys().map(String::as_str));
                }
                PatternSegment::SurfaceKey => match pattern.first() {
                    Some(PatternSegment::Literal("surfaces")) => {
                        append_segments(&mut paths, self.surfaces.keys().map(String::as_str))
                    }
                    Some(PatternSegment::Literal("windows")) => {
                        append_segments(&mut paths, self.windows.keys().map(String::as_str));
                    }
                    _ => return Vec::new(),
                },
            }
        }
        paths
    }
}

fn append_segments<'a>(
    paths: &mut Vec<String>,
    segments: impl IntoIterator<Item = &'a str> + Clone,
) {
    let existing = std::mem::take(paths);
    for path in existing {
        for segment in segments.clone() {
            paths.push(if path.is_empty() {
                segment.to_string()
            } else {
                format!("{path}.{segment}")
            });
        }
    }
}

fn select_map<T>(
    values: &BTreeMap<String, T>,
    path: &[&str],
    select: fn(&T, &[&str]) -> Option<Value>,
) -> Option<Value>
where
    T: Serialize,
{
    let Some((key, tail)) = path.split_first() else {
        return serialise_selected(values);
    };
    select(values.get(*key)?, tail)
}

fn map_node_kind<T>(
    values: &BTreeMap<String, T>,
    path: &[&str],
    node_kind: fn(&T, &[&str]) -> Option<SnapshotNodeKind>,
) -> Option<SnapshotNodeKind> {
    let Some((key, tail)) = path.split_first() else {
        return Some(SnapshotNodeKind::Object);
    };
    node_kind(values.get(*key)?, tail)
}

macro_rules! flat_snapshot {
    ($ty:ty, $($field:ident),+ $(,)?) => {
        impl $ty {
            fn select(&self, path: &[&str]) -> Option<Value> {
                match path {
                    [] => serialise_selected(self),
                    $([stringify!($field)] => serialise_selected(&self.$field),)+
                    _ => None,
                }
            }

            fn node_kind(&self, path: &[&str]) -> Option<SnapshotNodeKind> {
                match path {
                    [] => Some(SnapshotNodeKind::Object),
                    $([stringify!($field)] => Some(SnapshotNodeKind::Leaf),)+
                    _ => None,
                }
            }
        }
    };
}

flat_snapshot!(InfoSnapshot, service, version, backend, engine, instance);
#[cfg(feature = "xwayland")]
flat_snapshot!(XwaylandSnapshot, enabled, persist_path);
flat_snapshot!(
    WindowSnapshot,
    id,
    foreign_id,
    title,
    app_id,
    x,
    y,
    width,
    height,
    focused,
    maximized,
    minimized,
    output,
    band,
);
flat_snapshot!(
    FocusSnapshot,
    keyboard,
    exclusive_latch,
    pointer,
    pointer_grab,
    session_lock,
);
flat_snapshot!(DecorationSnapshot, enabled, style);
flat_snapshot!(BindingsSnapshot, enabled, profile, table);
flat_snapshot!(
    CornersSnapshot,
    enabled,
    deadzone_px,
    dwell_ms,
    velocity_max_px_s
);
flat_snapshot!(
    PortSnapshot,
    level,
    event_seq,
    lost_count,
    queue_depth,
    reply_timeouts,
    publish_timeouts,
    slug_collisions,
    broker,
);
flat_snapshot!(RectSnapshot, x, y, width, height);
flat_snapshot!(
    LayerSnapshot,
    stratum,
    interactivity,
    exclusive_zone,
    binding,
);

impl OutputSnapshot {
    fn select(&self, path: &[&str]) -> Option<Value> {
        match path {
            [] => serialise_selected(self),
            ["name"] => serialise_selected(&self.name),
            ["default"] => serialise_selected(&self.default),
            ["x"] => serialise_selected(&self.x),
            ["y"] => serialise_selected(&self.y),
            ["width"] => serialise_selected(&self.width),
            ["height"] => serialise_selected(&self.height),
            ["scale"] => serialise_selected(&self.scale),
            ["refresh_mhz"] => serialise_selected(&self.refresh_mhz),
            ["usable", tail @ ..] => self.usable.select(tail),
            _ => None,
        }
    }

    fn node_kind(&self, path: &[&str]) -> Option<SnapshotNodeKind> {
        match path {
            [] | ["usable"] => Some(SnapshotNodeKind::Object),
            ["name" | "default" | "x" | "y" | "width" | "height" | "scale" | "refresh_mhz"] => {
                Some(SnapshotNodeKind::Leaf)
            }
            ["usable", tail @ ..] => self.usable.node_kind(tail),
            _ => None,
        }
    }
}

impl InputSnapshot {
    fn select(&self, path: &[&str]) -> Option<Value> {
        match path {
            [] => serialise_selected(self),
            ["corners", tail @ ..] => self.corners.select(tail),
            _ => None,
        }
    }

    fn node_kind(&self, path: &[&str]) -> Option<SnapshotNodeKind> {
        match path {
            [] | ["corners"] => Some(SnapshotNodeKind::Object),
            ["corners", tail @ ..] => self.corners.node_kind(tail),
            _ => None,
        }
    }
}

impl SurfaceSnapshot {
    fn select(&self, path: &[&str]) -> Option<Value> {
        match path {
            [] => serialise_selected(self),
            ["id"] => serialise_selected(&self.id),
            ["role"] => serialise_selected(&self.role),
            ["mapped"] => serialise_selected(&self.mapped),
            ["visible"] => serialise_selected(&self.visible),
            ["x"] => serialise_selected(&self.x),
            ["y"] => serialise_selected(&self.y),
            ["width"] => serialise_selected(&self.width),
            ["height"] => serialise_selected(&self.height),
            ["band"] => serialise_selected(&self.band),
            ["sequence"] => serialise_selected(&self.sequence),
            ["tree_index"] => serialise_selected(&self.tree_index),
            ["parent"] => serialise_selected(&self.parent),
            ["output"] => serialise_selected(&self.output),
            ["title"] => serialise_selected(&self.title),
            ["app_id"] => serialise_selected(&self.app_id),
            ["focused"] => serialise_selected(&self.focused),
            ["activated"] => serialise_selected(&self.activated),
            ["maximized"] => serialise_selected(&self.maximized),
            ["minimized"] => serialise_selected(&self.minimized),
            ["decoration"] => serialise_selected(&self.decoration),
            ["layer"] => serialise_selected(&self.layer),
            ["layer", tail @ ..] => self.layer.as_ref()?.select(tail),
            ["foreign_id"] => serialise_selected(&self.foreign_id),
            _ => None,
        }
    }

    fn node_kind(&self, path: &[&str]) -> Option<SnapshotNodeKind> {
        match path {
            [] => Some(SnapshotNodeKind::Object),
            [
                "id" | "role" | "mapped" | "visible" | "x" | "y" | "width" | "height" | "band"
                | "sequence" | "tree_index" | "parent" | "output" | "title" | "app_id" | "focused"
                | "activated" | "maximized" | "minimized" | "decoration" | "foreign_id",
            ] => Some(SnapshotNodeKind::Leaf),
            ["layer"] => Some(if self.layer.is_some() {
                SnapshotNodeKind::Object
            } else {
                SnapshotNodeKind::Leaf
            }),
            ["layer", tail @ ..] => self.layer.as_ref()?.node_kind(tail),
            _ => None,
        }
    }
}

pub(super) struct OutputProjection {
    pub(super) rows: BTreeMap<String, OutputSnapshot>,
    pub(super) keys: Vec<(Output, String)>,
    pub(super) slug_collisions: u64,
}

pub(super) fn project_outputs(state: &WaylandState) -> Option<OutputProjection> {
    let sources = state.backend.port_outputs();
    let mut keys = Vec::<(Output, String)>::with_capacity(sources.len());
    let mut rows = BTreeMap::<String, OutputSnapshot>::new();
    let mut slug_collisions = 0_u64;
    for source in sources {
        let key = output_key(&source.name);
        if output_slug_collides(&rows, &key, &source.name, &mut slug_collisions) {
            continue;
        }
        let usable = state.port_usable_output_rect_for(&source.output)?;
        keys.push((source.output, key.clone()));
        rows.insert(
            key,
            OutputSnapshot {
                name: source.name,
                default: source.default,
                x: source.x,
                y: source.y,
                width: source.width,
                height: source.height,
                scale: source.scale,
                refresh_mhz: source.refresh_mhz,
                usable: RectSnapshot {
                    x: usable.x,
                    y: usable.y,
                    width: usable.width,
                    height: usable.height,
                },
            },
        );
    }
    Some(OutputProjection {
        rows,
        keys,
        slug_collisions,
    })
}

pub(super) fn project_output(
    state: &WaylandState,
    output: &Output,
) -> Option<(String, OutputSnapshot)> {
    let source = state.backend.port_output(output)?;
    let key = output_key(&source.name);
    let usable = state.port_usable_output_rect_for(&source.output)?;
    Some((
        key,
        OutputSnapshot {
            name: source.name,
            default: source.default,
            x: source.x,
            y: source.y,
            width: source.width,
            height: source.height,
            scale: source.scale,
            refresh_mhz: source.refresh_mhz,
            usable: RectSnapshot {
                x: usable.x,
                y: usable.y,
                width: usable.width,
                height: usable.height,
            },
        },
    ))
}

pub(super) fn project_surface_by_id(
    state: &WaylandState,
    id: SurfaceId,
    output_keys: &[(Output, String)],
) -> Option<SurfaceSnapshot> {
    let object = state.surface_objects.get(&id)?;
    let record = state.surfaces.get(object)?;
    (!matches!(record.role, SurfaceRole::Dormant(_)))
        .then(|| project_surface_row(state, record, output_keys, state.session_lock_active()))
}

fn project_surface_row(
    state: &WaylandState,
    record: &SurfaceRecord,
    output_keys: &[(Output, String)],
    session_lock_active: bool,
) -> SurfaceSnapshot {
    let redact_ordinary_surface = session_lock_active
        && (matches!(&state.lock_lifecycle, LockLifecycle::Unlocked)
            || !state.surface_is_session_presentable(record));
    let output = surface_output(state, record, state.backend.default_output().as_ref())
        .and_then(|output| output_key_for(output_keys, output));
    let layer = match &record.role {
        SurfaceRole::Layer(role) => Some(LayerSnapshot {
            stratum: layer_name(role.committed_layer),
            interactivity: interactivity_name(role.committed_keyboard_interactivity),
            exclusive_zone: exclusive_zone_value(role.surface.cached_state().exclusive_zone),
            binding: match role.output {
                LayerOutputBinding::Explicit(_) => "explicit",
                LayerOutputBinding::Default(_) | LayerOutputBinding::Closed => "default",
            },
        }),
        _ => None,
    };
    SurfaceSnapshot {
        id: record.id.0,
        role: record.role.kind(),
        mapped: record.mapped,
        visible: record.layout.visible && !redact_ordinary_surface,
        x: record.layout.x,
        y: record.layout.y,
        width: record.layout.width,
        height: record.layout.height,
        band: band_name(record.layout.z.band),
        sequence: record.layout.z.sequence,
        tree_index: record.layout.z.tree_index,
        parent: record.layout.parent.map(|id| id.0),
        output,
        title: (!redact_ordinary_surface)
            .then(|| record.title.clone())
            .flatten(),
        app_id: (!redact_ordinary_surface)
            .then(|| record.app_id.clone())
            .flatten(),
        focused: record.focused,
        activated: record.focused,
        maximized: record.committed_maximized,
        minimized: record.minimized,
        decoration: matches!(record.role, SurfaceRole::Toplevel(_))
            .then_some(decoration_name(record.committed_decoration)),
        layer,
        foreign_id: (record.mapped && matches!(record.role, SurfaceRole::Toplevel(_)))
            .then(|| state.foreign_toplevel_identifiers.get(&record.id).cloned())
            .flatten(),
    }
}

pub(super) fn project_window_row(surface: &SurfaceSnapshot) -> WindowSnapshot {
    WindowSnapshot {
        id: surface.id,
        foreign_id: surface.foreign_id.clone(),
        title: surface.title.clone(),
        app_id: surface.app_id.clone(),
        x: surface.x,
        y: surface.y,
        width: surface.width,
        height: surface.height,
        focused: surface.focused,
        maximized: surface.maximized,
        minimized: surface.minimized,
        output: surface.output.clone(),
        band: surface.band,
    }
}

pub(super) fn project_focus(state: &WaylandState) -> FocusSnapshot {
    let session_lock_active = state.session_lock_active();
    FocusSnapshot {
        keyboard: state
            .keyboard
            .current_focus()
            .and_then(|target| target.surface_id())
            .and_then(|object| state.surfaces.get(&object))
            .map(|record| record.id.0),
        exclusive_latch: state
            .exclusive_keyboard_focus
            .as_ref()
            .and_then(|object| state.surfaces.get(object))
            .map(|record| record.id.0),
        pointer: state
            .pointer
            .current_focus()
            .and_then(|target| target.surface_id())
            .and_then(|object| state.surfaces.get(&object))
            .map(|record| record.id.0),
        pointer_grab: pointer_grab_name(state),
        session_lock: if !session_lock_active {
            "none"
        } else {
            match &state.lock_lifecycle {
                LockLifecycle::Unlocked => "unlocking",
                LockLifecycle::Locking { .. } => "locking",
                LockLifecycle::Locked { .. } => "locked",
                LockLifecycle::OrphanedLocked { .. } => "orphaned",
            }
        },
    }
}

pub(super) fn project_stack(state: &WaylandState) -> Vec<u64> {
    let mut roots = state
        .surfaces
        .values()
        .filter(|record| {
            record.mapped
                && record.layout.parent.is_none()
                && !matches!(record.role, SurfaceRole::Dormant(_))
        })
        .collect::<Vec<_>>();
    roots.sort_by(|left, right| surface_stack_cmp(left, right).reverse());
    roots.into_iter().map(|record| record.id.0).collect()
}

/// Build one fully owned snapshot on the protocol thread.
pub(super) fn snapshot(state: &WaylandState, context: &SnapshotContext) -> Option<CompSnapshot> {
    // This is the same authority boundary used by foreign-toplevel
    // publication (`session_lock_active`) and renderer selection
    // (`surface_is_session_presentable`). Do not derive it from visibility.
    let session_lock_active = state.session_lock_active();
    let OutputProjection {
        rows: outputs,
        keys: output_keys,
        slug_collisions,
    } = project_outputs(state)?;

    let mut surfaces = BTreeMap::new();
    for record in state
        .surfaces
        .values()
        .filter(|record| !matches!(record.role, SurfaceRole::Dormant(_)))
    {
        let key = surface_key(record.id);
        surfaces.insert(
            key,
            project_surface_row(state, record, &output_keys, session_lock_active),
        );
    }

    let windows = if session_lock_active {
        BTreeMap::new()
    } else {
        surfaces
            .iter()
            .filter(|(_, surface)| surface.role == "toplevel" && surface.mapped)
            .map(|(key, surface)| (key.clone(), project_window_row(surface)))
            .collect()
    };

    let stack = project_stack(state);

    let bindings = state.bindings.port_snapshot();
    Some(CompSnapshot {
        info: InfoSnapshot {
            service: context.service.clone(),
            version: context.version.clone(),
            backend: context.backend,
            engine: context.engine,
            instance: context.instance.clone(),
        },
        outputs,
        surfaces,
        windows,
        stack,
        focus: project_focus(state),
        decoration: DecorationSnapshot {
            enabled: context.decoration_enabled,
            style: context.decoration_style,
        },
        bindings: BindingsSnapshot {
            enabled: bindings.enabled,
            profile: bindings.profile,
            table: bindings
                .table
                .into_iter()
                .map(|row| BindingRowSnapshot {
                    chord: row.chord,
                    action: row.action,
                })
                .collect(),
        },
        input: InputSnapshot {
            corners: state.observations.corner_config.into(),
        },
        #[cfg(feature = "xwayland")]
        xwayland: XwaylandSnapshot {
            enabled: state.xwayland.enabled,
            persist_path: Arc::from(
                super::xwayland::xwayland_enabled_persist_path(&state.xwayland.socket_name)
                    .display()
                    .to_string(),
            ),
        },
        port: PortSnapshot {
            level: "L2",
            event_seq: context.event_seq.load(Ordering::Acquire),
            lost_count: context.lost_count.load(Ordering::Acquire),
            queue_depth: context.queue_depth.load(Ordering::Acquire),
            reply_timeouts: context.reply_timeouts.load(Ordering::Acquire),
            publish_timeouts: context.publish_timeouts.load(Ordering::Acquire),
            slug_collisions,
            broker: if context.broker.load(Ordering::Acquire) == BROKER_CONNECTED {
                "connected"
            } else {
                "retrying"
            },
        },
        full_tree: tokio::sync::OnceCell::new(),
    })
}

fn output_slug_collides(
    outputs: &BTreeMap<String, OutputSnapshot>,
    key: &str,
    dropped_output: &str,
    collisions: &mut u64,
) -> bool {
    let Some(first) = outputs.get(key) else {
        return false;
    };
    *collisions = collisions.saturating_add(1);
    // Snapshotting is a calloop service point: keep this path free of shared
    // mutable statics and locks. The snapshot counter is authoritative.
    tracing::debug!(
        slug = key,
        kept_output = %first.name,
        dropped_output,
        "compositor Bus output slug collision; keeping first output"
    );
    true
}

fn surface_output<'a>(
    state: &'a WaylandState,
    record: &'a SurfaceRecord,
    default: Option<&'a Output>,
) -> Option<&'a Output> {
    let mut current = record;
    while let Some(parent) = current.layout.parent {
        let object = state.surface_objects.get(&parent)?;
        current = state.surfaces.get(object)?;
    }
    match &current.role {
        SurfaceRole::Layer(role) => role.output.output(),
        SurfaceRole::LockSurface(role) => Some(&role.output),
        SurfaceRole::Toplevel(_) => default,
        #[cfg(feature = "xwayland")]
        SurfaceRole::X11(_) => default,
        SurfaceRole::Popup(_)
        | SurfaceRole::ImePopup(_)
        | SurfaceRole::Subsurface { .. }
        | SurfaceRole::Dormant(_) => None,
    }
}

fn output_key_for(outputs: &[(Output, String)], requested: &Output) -> Option<String> {
    outputs
        .iter()
        .find(|(output, _)| output == requested)
        .map(|(_, key)| key.clone())
}

pub(crate) fn output_key(name: &str) -> String {
    let mut key = String::from("o_");
    for character in name.chars() {
        if character.is_ascii_alphanumeric() {
            key.push(character.to_ascii_lowercase());
        } else {
            key.push('_');
        }
    }
    key
}

fn surface_key(id: SurfaceId) -> String {
    format!("s{}", id.0)
}

fn band_name(band: StackBand) -> &'static str {
    match band {
        StackBand::Background => "background",
        StackBand::Bottom => "bottom",
        StackBand::Normal => "normal",
        StackBand::Top => "top",
        StackBand::Overlay => "overlay",
        StackBand::Lock => "lock",
    }
}

fn layer_name(layer: WlrLayer) -> &'static str {
    match layer {
        WlrLayer::Background => "background",
        WlrLayer::Bottom => "bottom",
        WlrLayer::Top => "top",
        WlrLayer::Overlay => "overlay",
    }
}

fn interactivity_name(interactivity: KeyboardInteractivity) -> &'static str {
    match interactivity {
        KeyboardInteractivity::None => "none",
        KeyboardInteractivity::OnDemand => "on_demand",
        KeyboardInteractivity::Exclusive => "exclusive",
    }
}

fn exclusive_zone_value(zone: ExclusiveZone) -> i32 {
    match zone {
        ExclusiveZone::Exclusive(amount) => i32::try_from(amount).map_or(i32::MAX, |value| value),
        ExclusiveZone::Neutral => 0,
        ExclusiveZone::DontCare => -1,
    }
}

fn decoration_name(mode: SceneDecorationMode) -> &'static str {
    match mode {
        SceneDecorationMode::ServerSide => "server",
        SceneDecorationMode::ClientSide => "client",
        SceneDecorationMode::Unbound => "unbound",
    }
}

fn pointer_grab_name(state: &WaylandState) -> &'static str {
    if let Some(grab) = &state.chrome_pointer_grab {
        return match grab.kind {
            ChromePointerGrabKind::Button(_) => "chrome",
            ChromePointerGrabKind::Move => "move",
            ChromePointerGrabKind::Resize(_) => "resize",
        };
    }
    if let Some(interaction) = &state.interactive_pointer {
        return match interaction {
            InteractivePointer::Move { .. } => "move",
            InteractivePointer::Resize { .. } => "resize",
        };
    }
    if state.pointer.is_grabbed() {
        "popup"
    } else {
        "none"
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DescribeType {
    Bool,
    Number,
    String,
    List,
    Object,
}

impl DescribeType {
    const fn name(self) -> &'static str {
        match self {
            Self::Bool => "bool",
            Self::Number => "number",
            Self::String => "string",
            Self::List => "list",
            Self::Object => "object",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PatternSegment {
    Literal(&'static str),
    OutputKey,
    SurfaceKey,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct DescribeEntry {
    pub(crate) pattern: &'static [PatternSegment],
    pub(crate) ty: DescribeType,
    pub(crate) description: &'static str,
    pub(crate) mutable: bool,
    pub(crate) sensitive: bool,
    pub(crate) format: Option<&'static str>,
    pub(crate) enum_values: &'static [&'static str],
    pub(crate) range: Option<&'static str>,
    pub(crate) persistence: Option<&'static str>,
    pub(crate) owner: &'static str,
}

macro_rules! descriptor {
    ($segments:expr, $ty:ident, $description:expr) => {
        DescribeEntry {
            pattern: $segments,
            ty: DescribeType::$ty,
            description: $description,
            mutable: false,
            sensitive: false,
            format: None,
            enum_values: &[],
            range: None,
            persistence: None,
            owner: "comp",
        }
    };
    ($segments:expr, $ty:ident, $description:expr, mutable, range = $range:expr) => {
        DescribeEntry {
            mutable: true,
            range: Some($range),
            persistence: Some("none"),
            ..descriptor!($segments, $ty, $description)
        }
    };
    ($segments:expr, $ty:ident, $description:expr, mutable) => {
        DescribeEntry {
            mutable: true,
            persistence: Some("none"),
            ..descriptor!($segments, $ty, $description)
        }
    };
    ($segments:expr, $ty:ident, $description:expr, format = $format:expr) => {
        DescribeEntry {
            format: Some($format),
            ..descriptor!($segments, $ty, $description)
        }
    };
    ($segments:expr, $ty:ident, $description:expr, enum = $values:expr) => {
        DescribeEntry {
            enum_values: $values,
            ..descriptor!($segments, $ty, $description)
        }
    };
}

use PatternSegment::{Literal as L, OutputKey as O, SurfaceKey as S};

pub(crate) static DESCRIPTORS: &[DescribeEntry] = &[
    descriptor!(
        &[L("info"), L("service")],
        String,
        "Registered Bus service name"
    ),
    descriptor!(
        &[L("info"), L("version")],
        String,
        "Compositor build version"
    ),
    descriptor!(&[L("info"), L("backend")], String, "Active compositor backend", enum = &["nested", "kms"]),
    descriptor!(&[L("info"), L("engine")], String, "Rendering engine"),
    descriptor!(
        &[L("info"), L("instance")],
        String,
        "Random per-process compositor instance id"
    ),
    descriptor!(
        &[L("outputs"), O, L("name")],
        String,
        "Raw protocol output name"
    ),
    descriptor!(
        &[L("outputs"), O, L("default")],
        Bool,
        "Whether this is the default output"
    ),
    descriptor!(
        &[L("outputs"), O, L("x")],
        Number,
        "Logical output x origin",
        format = "logical_px"
    ),
    descriptor!(
        &[L("outputs"), O, L("y")],
        Number,
        "Logical output y origin",
        format = "logical_px"
    ),
    descriptor!(
        &[L("outputs"), O, L("width")],
        Number,
        "Logical output width",
        format = "logical_px"
    ),
    descriptor!(
        &[L("outputs"), O, L("height")],
        Number,
        "Logical output height",
        format = "logical_px"
    ),
    descriptor!(
        &[L("outputs"), O, L("scale")],
        Number,
        "Fractional output scale",
        format = "scale_factor"
    ),
    descriptor!(
        &[L("outputs"), O, L("refresh_mhz")],
        Number,
        "Output refresh rate",
        format = "millihertz"
    ),
    descriptor!(
        &[L("outputs"), O, L("usable"), L("x")],
        Number,
        "Usable logical x origin",
        format = "logical_px"
    ),
    descriptor!(
        &[L("outputs"), O, L("usable"), L("y")],
        Number,
        "Usable logical y origin",
        format = "logical_px"
    ),
    descriptor!(
        &[L("outputs"), O, L("usable"), L("width")],
        Number,
        "Usable logical width",
        format = "logical_px"
    ),
    descriptor!(
        &[L("outputs"), O, L("usable"), L("height")],
        Number,
        "Usable logical height",
        format = "logical_px"
    ),
    descriptor!(
        &[L("surfaces"), S, L("id")],
        Number,
        "Session-local surface id",
        format = "surface_id"
    ),
    descriptor!(&[L("surfaces"), S, L("role")], String, "Wayland surface role", enum = &["toplevel", "popup", "layer", "subsurface", "lock"]),
    descriptor!(
        &[L("surfaces"), S, L("mapped")],
        Bool,
        "Whether the surface has mapped protocol content"
    ),
    descriptor!(
        &[L("surfaces"), S, L("visible")],
        Bool,
        "Effective scene visibility including ancestors"
    ),
    descriptor!(
        &[L("surfaces"), S, L("x")],
        Number,
        "Surface x origin",
        format = "logical_px"
    ),
    descriptor!(
        &[L("surfaces"), S, L("y")],
        Number,
        "Surface y origin",
        format = "logical_px"
    ),
    descriptor!(
        &[L("surfaces"), S, L("width")],
        Number,
        "Surface width",
        format = "logical_px"
    ),
    descriptor!(
        &[L("surfaces"), S, L("height")],
        Number,
        "Surface height",
        format = "logical_px"
    ),
    descriptor!(&[L("surfaces"), S, L("band")], String, "Compositor stack band", enum = &["background", "bottom", "normal", "top", "overlay", "lock"]),
    descriptor!(
        &[L("surfaces"), S, L("sequence")],
        Number,
        "Root ordering sequence"
    ),
    descriptor!(
        &[L("surfaces"), S, L("tree_index")],
        Number,
        "Within-tree ordering index"
    ),
    descriptor!(
        &[L("surfaces"), S, L("parent")],
        Number,
        "Parent surface id or null",
        format = "surface_id"
    ),
    descriptor!(
        &[L("surfaces"), S, L("output")],
        String,
        "Output key or null"
    ),
    descriptor!(
        &[L("surfaces"), S, L("title")],
        String,
        "Cached toplevel title or null"
    ),
    descriptor!(
        &[L("surfaces"), S, L("app_id")],
        String,
        "Cached toplevel app id or null"
    ),
    descriptor!(
        &[L("surfaces"), S, L("focused")],
        Bool,
        "Current focus-arbiter decision"
    ),
    descriptor!(
        &[L("surfaces"), S, L("activated")],
        Bool,
        "XDG activation decision from the same focus edge"
    ),
    descriptor!(
        &[L("surfaces"), S, L("maximized")],
        Bool,
        "Committed maximized state"
    ),
    descriptor!(
        &[L("surfaces"), S, L("minimized")],
        Bool,
        "Compositor minimized state"
    ),
    descriptor!(&[L("surfaces"), S, L("decoration")], String, "Committed decoration mode or null", enum = &["server", "client", "unbound"]),
    descriptor!(
        &[L("surfaces"), S, L("layer")],
        Object,
        "Layer metadata object or null"
    ),
    descriptor!(&[L("surfaces"), S, L("layer"), L("stratum")], String, "Committed layer-shell stratum", enum = &["background", "bottom", "top", "overlay"]),
    descriptor!(&[L("surfaces"), S, L("layer"), L("interactivity")], String, "Committed layer keyboard interactivity", enum = &["none", "on_demand", "exclusive"]),
    descriptor!(
        &[L("surfaces"), S, L("layer"), L("exclusive_zone")],
        Number,
        "Applied layer exclusive zone",
        format = "logical_px"
    ),
    descriptor!(&[L("surfaces"), S, L("layer"), L("binding")], String, "Layer output binding", enum = &["explicit", "default"]),
    descriptor!(
        &[L("surfaces"), S, L("foreign_id")],
        String,
        "Mapped foreign-toplevel identifier or null"
    ),
    descriptor!(
        &[L("windows"), S, L("id")],
        Number,
        "Session-local toplevel id",
        format = "surface_id"
    ),
    descriptor!(
        &[L("windows"), S, L("foreign_id")],
        String,
        "Mapped foreign-toplevel identifier"
    ),
    descriptor!(
        &[L("windows"), S, L("title")],
        String,
        "Cached toplevel title"
    ),
    descriptor!(
        &[L("windows"), S, L("app_id")],
        String,
        "Cached toplevel app id"
    ),
    descriptor!(
        &[L("windows"), S, L("x")],
        Number,
        "Toplevel x origin",
        format = "logical_px"
    ),
    descriptor!(
        &[L("windows"), S, L("y")],
        Number,
        "Toplevel y origin",
        format = "logical_px"
    ),
    descriptor!(
        &[L("windows"), S, L("width")],
        Number,
        "Toplevel width",
        format = "logical_px"
    ),
    descriptor!(
        &[L("windows"), S, L("height")],
        Number,
        "Toplevel height",
        format = "logical_px"
    ),
    descriptor!(
        &[L("windows"), S, L("focused")],
        Bool,
        "Whether this toplevel owns keyboard focus"
    ),
    descriptor!(
        &[L("windows"), S, L("maximized")],
        Bool,
        "Committed maximized state"
    ),
    descriptor!(
        &[L("windows"), S, L("minimized")],
        Bool,
        "Compositor minimized state"
    ),
    descriptor!(
        &[L("windows"), S, L("output")],
        String,
        "Output key or null"
    ),
    descriptor!(&[L("windows"), S, L("band")], String, "Compositor stack band; writable as bottom|normal to demote a window behind all normal windows or restore it", enum = &["background", "bottom", "normal", "top", "overlay", "lock"]),
    descriptor!(
        &[L("stack")],
        List,
        "Mapped root surface ids from top to bottom",
        format = "surface_id"
    ),
    descriptor!(
        &[L("focus"), L("keyboard")],
        Number,
        "Keyboard-focused surface id or null",
        format = "surface_id"
    ),
    descriptor!(
        &[L("focus"), L("exclusive_latch")],
        Number,
        "Exclusive layer focus latch or null",
        format = "surface_id"
    ),
    descriptor!(
        &[L("focus"), L("pointer")],
        Number,
        "Pointer-focused surface id or null",
        format = "surface_id"
    ),
    descriptor!(&[L("focus"), L("pointer_grab")], String, "Active pointer grab kind", enum = &["none", "chrome", "move", "resize", "popup"]),
    descriptor!(&[L("focus"), L("session_lock")], String, "Session-lock observation state", enum = &["none", "locking", "locked", "orphaned", "unlocking"]),
    descriptor!(
        &[L("decoration"), L("enabled")],
        Bool,
        "Whether server-side decoration is enabled"
    ),
    descriptor!(&[L("decoration"), L("style")], String, "Startup decoration style", enum = &["mac", "win11", "cosmix"]),
    descriptor!(
        &[L("bindings"), L("enabled")],
        Bool,
        "Whether normal compositor key interception is enabled"
    ),
    descriptor!(&[L("bindings"), L("profile")], String, "Compiled binding profile", enum = &["nested", "kms-live"]),
    descriptor!(
        &[L("bindings"), L("table")],
        List,
        "Compiled keybinding chord/action rows"
    ),
    descriptor!(
        &[L("input"), L("corners"), L("enabled")],
        Bool,
        "Whether compositor hot-corner detection is enabled",
        mutable
    ),
    descriptor!(
        &[L("input"), L("corners"), L("deadzone_px")],
        Number,
        "Corner deadzone in logical pixels",
        mutable,
        range = "1.0..=256.0"
    ),
    descriptor!(
        &[L("input"), L("corners"), L("dwell_ms")],
        Number,
        "Velocity-qualified corner dwell in milliseconds",
        mutable,
        range = "0..=5000"
    ),
    descriptor!(
        &[L("input"), L("corners"), L("velocity_max_px_s")],
        Number,
        "Maximum corner-entry velocity in logical pixels per second",
        mutable,
        range = "1.0..=20000.0"
    ),
    // The one file-persisted leaf on this surface (see the resolver in
    // xwayland.rs for why startup-read + persistence:none would make the
    // leaf decorative). `persistence: "file"` overrides the mutable
    // macro-arm's "none".
    #[cfg(feature = "xwayland")]
    DescribeEntry {
        persistence: Some("file"),
        ..descriptor!(
            &[L("xwayland"), L("enabled")],
            Bool,
            "Whether this compositor spawns XWayland; read at startup, a write persists \
             for the NEXT startup (no live toggle; the Set reply's `persisted` field \
             reports write durability). COSMIX_COMP_XWAYLAND overrides at launch",
            mutable
        )
    },
    #[cfg(feature = "xwayland")]
    descriptor!(
        &[L("xwayland"), L("persist_path")],
        String,
        "Resolved per-socket file xwayland.enabled persists to (root- and \
         socket-dependent; read-only so the governing file is visible, not deduced)"
    ),
    descriptor!(&[L("port"), L("level")], String, "Implemented property substrate level", enum = &["L2"]),
    descriptor!(
        &[L("port"), L("event_seq")],
        Number,
        "Global compositor observation event sequence"
    ),
    descriptor!(
        &[L("port"), L("lost_count")],
        Number,
        "Cumulative compositor observation records lost"
    ),
    descriptor!(
        &[L("port"), L("queue_depth")],
        Number,
        "Accepted port reads and controls not yet completed"
    ),
    descriptor!(
        &[L("port"), L("reply_timeouts")],
        Number,
        "Reply send abandoned after 2 s; delivery not guaranteed (the client sink may still flush it); also counts saturated reply lanes"
    ),
    descriptor!(
        &[L("port"), L("publish_timeouts")],
        Number,
        "Topic publication failures and timeouts"
    ),
    descriptor!(
        &[L("port"), L("slug_collisions")],
        Number,
        "Outputs omitted because their public slug collided with an earlier output"
    ),
    descriptor!(&[L("port"), L("broker")], String, "Live broker connection state", enum = &["connected", "retrying"]),
];

impl DescribeEntry {
    fn matches(self, path: &PropPath) -> bool {
        let segments = path.segments().collect::<Vec<_>>();
        segments.len() == self.pattern.len()
            && segments
                .iter()
                .zip(self.pattern)
                .all(|(actual, expected)| match expected {
                    PatternSegment::Literal(expected) => actual == expected,
                    PatternSegment::OutputKey => actual.starts_with("o_") && actual.len() > 2,
                    PatternSegment::SurfaceKey => actual.strip_prefix('s').is_some_and(|id| {
                        !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit())
                    }),
                })
    }
}

#[derive(Serialize)]
struct DescribeReply<'a> {
    path: &'a str,
    #[serde(rename = "type")]
    ty: &'a str,
    mutable: bool,
    sensitive: bool,
    description: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    format: Option<&'a str>,
    #[serde(rename = "enum", skip_serializing_if = "slice_is_empty")]
    enum_values: &'a [&'a str],
    #[serde(skip_serializing_if = "Option::is_none")]
    range: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    persistence: Option<&'a str>,
    owner: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    children: Option<Vec<String>>,
}

fn slice_is_empty(values: &&[&str]) -> bool {
    values.is_empty()
}

pub(super) fn service_requests(state: &mut WaylandState) {
    if state.pending_port_requests.is_empty() {
        return;
    }

    let stable =
        state.pointer_hit_test_batch_depth == 0 && !state.pointer_hit_test_transaction_applying;
    debug_assert!(
        stable,
        "Bus snapshot attempted inside a protocol transaction or hit-test batch"
    );
    if !stable {
        state.pending_port_requests.clear();
        return;
    }
    let Some(context) = state.port_context.clone() else {
        state.pending_port_requests.clear();
        return;
    };
    let Some(snapshot) = snapshot(state, &context).map(Arc::new) else {
        tracing::warn!(
            "compositor Bus snapshot contains coordinates not exactly representable as f32"
        );
        state.pending_port_requests.clear();
        return;
    };
    for request in state.pending_port_requests.drain(..) {
        let _ = request.reply.send(Arc::clone(&snapshot));
    }
}

pub(crate) async fn dispatch_read(
    snapshot: Arc<CompSnapshot>,
    command: String,
    args: Value,
) -> (u8, Arc<str>) {
    dispatch_read_with_limit(snapshot, command, args, MAX_REPLY_BODY_BYTES).await
}

async fn dispatch_read_with_limit(
    snapshot: Arc<CompSnapshot>,
    command: String,
    args: Value,
    limit_bytes: usize,
) -> (u8, Arc<str>) {
    if command == "comp.info" {
        return enforce_reply_limit(
            (
                0,
                Arc::from(
                    json!({
                        "service": snapshot.info.service,
                        "version": snapshot.info.version,
                        "backend": snapshot.info.backend,
                        "engine": snapshot.info.engine,
                        "output_count": snapshot.outputs.len(),
                        "surface_count": snapshot.surfaces.len(),
                        "event_seq": snapshot.port.event_seq,
                        "lost_count": snapshot.port.lost_count,
                    })
                    .to_string(),
                ),
            ),
            limit_bytes,
        );
    }
    if !matches!(
        command.as_str(),
        "comp.props.get" | "comp.props.list" | "comp.props.describe"
    ) {
        return error("unknown_verb");
    }
    if command == "comp.props.get" {
        match optional_path(&args, "path") {
            Ok(None) => {
                return full_tree(snapshot).await.map_or_else(
                    |()| error("busy"),
                    |reply| enforce_measured_reply_limit(reply, limit_bytes),
                );
            }
            Ok(Some(_)) => {}
            Err(()) => return error("unknown_path"),
        }
    }
    let reply =
        tokio::task::spawn_blocking(move || dispatch_selected_read(&snapshot, &command, &args))
            .await
            .unwrap_or_else(|_| error("busy"));
    enforce_reply_limit(reply, limit_bytes)
}

async fn full_tree(snapshot: Arc<CompSnapshot>) -> Result<SerialisedReply, ()> {
    static SERIALISATION_PERMIT: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();
    let snapshot_for_initialiser = Arc::clone(&snapshot);
    snapshot
        .full_tree
        .get_or_try_init(|| async move {
            let serialisation_permit = SERIALISATION_PERMIT
                .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(1)))
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| ())?;
            let body = tokio::task::spawn_blocking(move || {
                let _serialisation_permit = serialisation_permit;
                serde_json::to_string(snapshot_for_initialiser.as_ref())
                    .map(|body| {
                        let bytes = body.len();
                        SerialisedReply {
                            body: Arc::from(body),
                            bytes,
                        }
                    })
                    .map_err(|_| ())
            })
            .await
            .map_err(|error| {
                tracing::error!(%error, "compositor Bus full-tree serialiser task failed");
            })??;
            Ok(body)
        })
        .await
        .cloned()
}

fn enforce_reply_limit((rc, body): (u8, Arc<str>), limit_bytes: usize) -> (u8, Arc<str>) {
    if rc == 0 && body.len() > limit_bytes {
        too_large(limit_bytes)
    } else {
        (rc, body)
    }
}

fn enforce_measured_reply_limit(reply: SerialisedReply, limit_bytes: usize) -> (u8, Arc<str>) {
    if reply.bytes > limit_bytes {
        too_large(limit_bytes)
    } else {
        (0, reply.body)
    }
}

pub(crate) fn too_large(limit_bytes: usize) -> (u8, Arc<str>) {
    (
        10,
        Arc::from(
            json!({
                "error": "too_large",
                "limit_bytes": limit_bytes,
                "hint": "read a subtree",
            })
            .to_string(),
        ),
    )
}

fn dispatch_selected_read(snapshot: &CompSnapshot, command: &str, args: &Value) -> (u8, Arc<str>) {
    match command {
        "comp.props.get" => match optional_path(args, "path") {
            Ok(Some(path)) => {
                let segments = path.segments().collect::<Vec<_>>();
                snapshot.select(&segments).map_or_else(
                    || error("unknown_path"),
                    |value| (0, Arc::from(value.to_string())),
                )
            }
            Ok(None) => error("busy"),
            Err(()) => error("unknown_path"),
        },
        "comp.props.list" => match optional_path(args, "prefix") {
            Ok(prefix) => {
                let leaves = snapshot.leaf_paths();
                let paths = match prefix {
                    None => leaves,
                    Some(prefix) => leaves
                        .into_iter()
                        .filter(|leaf| leaf.starts_with(&prefix))
                        .collect(),
                };
                (0, Arc::from(json!(paths).to_string()))
            }
            Err(()) => error("unknown_path"),
        },
        "comp.props.describe" => match required_path(args, "path") {
            Ok(path) => describe(snapshot, &path)
                .map_or_else(|| error("unknown_path"), |body| (0, Arc::from(body))),
            Err(()) => error("unknown_path"),
        },
        _ => error("unknown_verb"),
    }
}

fn optional_path(args: &Value, key: &str) -> Result<Option<PropPath>, ()> {
    if args.is_null() {
        return Ok(None);
    }
    let object = args.as_object().ok_or(())?;
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(path)) => PropPath::new(path.clone()).map(Some).map_err(|_| ()),
        Some(_) => Err(()),
    }
}

fn required_path(args: &Value, key: &str) -> Result<PropPath, ()> {
    optional_path(args, key)?.ok_or(())
}

#[cfg(test)]
pub(crate) fn flattened_paths(tree: &Value) -> Vec<PropPath> {
    let mut paths = Vec::new();
    flatten_into(tree, "", &mut paths);
    paths
}

#[cfg(test)]
fn flatten_into(value: &Value, prefix: &str, paths: &mut Vec<PropPath>) {
    if let Value::Object(object) = value {
        for (key, child) in object {
            let path = if prefix.is_empty() {
                key.clone()
            } else {
                format!("{prefix}.{key}")
            };
            flatten_into(child, &path, paths);
        }
    } else if let Ok(path) = PropPath::new(prefix) {
        paths.push(path);
    }
}

fn describe(snapshot: &CompSnapshot, path: &PropPath) -> Option<String> {
    let segments = path.segments().collect::<Vec<_>>();
    let node_kind = snapshot.node_kind(&segments)?;
    let matches = DESCRIPTORS
        .iter()
        .copied()
        .filter(|entry| entry.matches(path))
        .collect::<Vec<_>>();
    if node_kind == SnapshotNodeKind::Leaf {
        let [entry] = matches.as_slice() else {
            return None;
        };
        return serde_json::to_string(&DescribeReply {
            path: path.as_str(),
            ty: entry.ty.name(),
            mutable: entry.mutable,
            sensitive: entry.sensitive,
            description: entry.description,
            format: entry.format,
            enum_values: entry.enum_values,
            range: entry.range,
            persistence: entry.persistence,
            owner: entry.owner,
            children: None,
        })
        .ok();
    }

    let leaves = snapshot.leaf_paths();
    let mut children = BTreeSet::new();
    let prefix_len = path.segments().count();
    for leaf in leaves.into_iter().filter(|leaf| leaf.starts_with(path)) {
        if let Some(child) = leaf.segments().nth(prefix_len) {
            children.insert(format!("{}.{}", path.as_str(), child));
        }
    }
    serde_json::to_string(&DescribeReply {
        path: path.as_str(),
        ty: "object",
        mutable: false,
        sensitive: false,
        description: "Compositor property subtree",
        format: None,
        enum_values: &[],
        range: None,
        persistence: None,
        owner: "comp",
        children: Some(children.into_iter().collect()),
    })
    .ok()
}

pub(crate) fn error(reason: &'static str) -> (u8, Arc<str>) {
    (10, Arc::from(json!({"error": reason}).to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> CompSnapshot {
        let output = "o_dp_1".to_string();
        let mut outputs = BTreeMap::new();
        outputs.insert(
            output.clone(),
            OutputSnapshot {
                name: "DP-1".into(),
                default: true,
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
                scale: 1.0,
                refresh_mhz: 60_000,
                usable: RectSnapshot {
                    x: 0.0,
                    y: 30.0,
                    width: 1920.0,
                    height: 1050.0,
                },
            },
        );
        let layer = SurfaceSnapshot {
            id: 1,
            role: "layer",
            mapped: true,
            visible: true,
            x: 0.0,
            y: 0.0,
            width: 1920.0,
            height: 30.0,
            band: "top",
            sequence: 2,
            tree_index: 0,
            parent: None,
            output: Some(output.clone()),
            title: None,
            app_id: None,
            focused: false,
            activated: false,
            maximized: false,
            minimized: false,
            decoration: None,
            layer: Some(LayerSnapshot {
                stratum: "top",
                interactivity: "exclusive",
                exclusive_zone: 30,
                binding: "explicit",
            }),
            foreign_id: None,
        };
        let toplevel = SurfaceSnapshot {
            id: 2,
            role: "toplevel",
            mapped: true,
            visible: true,
            x: 40.0,
            y: 60.0,
            width: 800.0,
            height: 600.0,
            band: "normal",
            sequence: 1,
            tree_index: 0,
            parent: None,
            output: Some(output.clone()),
            title: Some(Arc::from("Terminal")),
            app_id: Some(Arc::from("org.example.Terminal")),
            focused: true,
            activated: true,
            maximized: false,
            minimized: false,
            decoration: Some("server"),
            layer: None,
            foreign_id: Some("foreign-2".into()),
        };
        let mut surfaces = BTreeMap::new();
        surfaces.insert("s1".into(), layer);
        surfaces.insert("s2".into(), toplevel.clone());
        let mut popup = toplevel.clone();
        popup.id = 3;
        popup.role = "popup";
        popup.parent = Some(2);
        popup.title = None;
        popup.app_id = None;
        popup.focused = false;
        popup.activated = false;
        popup.decoration = None;
        popup.foreign_id = None;
        surfaces.insert("s3".into(), popup);
        let mut subsurface = toplevel.clone();
        subsurface.id = 4;
        subsurface.role = "subsurface";
        subsurface.mapped = false;
        subsurface.visible = false;
        subsurface.parent = Some(2);
        subsurface.title = None;
        subsurface.app_id = None;
        subsurface.focused = false;
        subsurface.activated = false;
        subsurface.decoration = None;
        subsurface.foreign_id = None;
        surfaces.insert("s4".into(), subsurface);
        let mut lock = toplevel.clone();
        lock.id = 5;
        lock.role = "lock";
        lock.band = "lock";
        lock.title = None;
        lock.app_id = None;
        lock.focused = false;
        lock.activated = false;
        lock.decoration = None;
        lock.foreign_id = None;
        surfaces.insert("s5".into(), lock);
        let mut windows = BTreeMap::new();
        windows.insert(
            "s2".into(),
            WindowSnapshot {
                id: toplevel.id,
                foreign_id: toplevel.foreign_id.clone(),
                title: toplevel.title.clone(),
                app_id: toplevel.app_id.clone(),
                x: toplevel.x,
                y: toplevel.y,
                width: toplevel.width,
                height: toplevel.height,
                focused: toplevel.focused,
                maximized: toplevel.maximized,
                minimized: toplevel.minimized,
                output: toplevel.output.clone(),
                band: toplevel.band,
            },
        );
        CompSnapshot {
            info: InfoSnapshot {
                service: Arc::from("comp-nested"),
                version: Arc::from("0.37.0"),
                backend: "nested",
                engine: "bevy-0.19/wgpu",
                instance: Arc::from("fixture"),
            },
            outputs,
            surfaces,
            windows,
            stack: vec![1, 2],
            focus: FocusSnapshot {
                keyboard: Some(2),
                exclusive_latch: Some(1),
                pointer: Some(2),
                pointer_grab: "none",
                session_lock: "none",
            },
            decoration: DecorationSnapshot {
                enabled: true,
                style: "mac",
            },
            bindings: BindingsSnapshot {
                enabled: true,
                profile: "nested",
                table: vec![BindingRowSnapshot {
                    chord: "Super+Q".into(),
                    action: "close-focused",
                }],
            },
            input: InputSnapshot {
                corners: CornerConfig::default().into(),
            },
            #[cfg(feature = "xwayland")]
            xwayland: XwaylandSnapshot {
                enabled: true,
                persist_path: Arc::from("/tmp/fixture/etc/comp/xwayland-enabled.comp-nested"),
            },
            port: PortSnapshot {
                level: "L2",
                event_seq: 0,
                lost_count: 0,
                queue_depth: 1,
                reply_timeouts: 0,
                publish_timeouts: 0,
                slug_collisions: 0,
                broker: "connected",
            },
            full_tree: tokio::sync::OnceCell::new(),
        }
    }

    #[test]
    fn corner_descriptors_are_the_only_mutable_process_lifetime_leaves() {
        let snapshot = fixture();
        let mutable = DESCRIPTORS
            .iter()
            .filter(|descriptor| descriptor.mutable)
            .collect::<Vec<_>>();
        // The corner leaves are process-lifetime (persistence "none");
        // `xwayland.enabled` is deliberately the surface's ONE
        // file-persisted mutable leaf (startup-read — a non-persisted
        // startup switch would be unreachable from its own surface).
        #[cfg(feature = "xwayland")]
        assert_eq!(mutable.len(), 5);
        #[cfg(not(feature = "xwayland"))]
        assert_eq!(mutable.len(), 4);
        for path in [
            "input.corners.enabled",
            "input.corners.deadzone_px",
            "input.corners.dwell_ms",
            "input.corners.velocity_max_px_s",
        ] {
            let path = PropPath::new(path).unwrap();
            let body = describe(&snapshot, &path).expect("mutable descriptor");
            let body = serde_json::from_str::<Value>(&body).unwrap();
            assert_eq!(body["mutable"], true);
            assert_eq!(body["persistence"], "none");
        }
        #[cfg(feature = "xwayland")]
        {
            let body = describe(&snapshot, &PropPath::new("xwayland.enabled").unwrap())
                .expect("xwayland.enabled descriptor");
            let body = serde_json::from_str::<Value>(&body).unwrap();
            assert_eq!(body["mutable"], true);
            assert_eq!(body["persistence"], "file");
            assert_eq!(body["type"], "bool");
        }
        let dwell = describe(&snapshot, &PropPath::new("input.corners.dwell_ms").unwrap()).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&dwell).unwrap()["range"],
            "0..=5000"
        );
    }

    #[test]
    fn descriptor_table_and_serialised_fixture_have_exact_parity() {
        let tree = serde_json::to_value(fixture()).expect("fixture serialises");
        let leaves = flattened_paths(&tree);
        for leaf in &leaves {
            let matches = DESCRIPTORS
                .iter()
                .filter(|entry| entry.matches(leaf))
                .count();
            assert_eq!(matches, 1, "descriptor count for {}", leaf.as_str());
        }
        for descriptor in DESCRIPTORS {
            assert!(
                leaves.iter().any(|leaf| descriptor.matches(leaf)),
                "descriptor has no fixture leaf: {:?}",
                descriptor.pattern
            );
        }
    }

    #[tokio::test]
    async fn list_uses_segment_ancestry_and_every_leaf_round_trips() {
        let snapshot = fixture();
        let tree = serde_json::to_value(&snapshot).expect("fixture serialises");
        let leaves = flattened_paths(&tree);
        let prefix = PropPath::new("surfaces.s1").expect("valid prefix");
        let expected = leaves
            .iter()
            .filter(|leaf| leaf.starts_with(&prefix))
            .map(|leaf| leaf.as_str().to_string())
            .collect::<Vec<_>>();
        let snapshot = Arc::new(snapshot);
        let (rc, body) = dispatch_read(
            Arc::clone(&snapshot),
            "comp.props.list".into(),
            json!({"prefix": "surfaces.s1"}),
        )
        .await;
        assert_eq!(rc, 0);
        assert_eq!(
            serde_json::from_str::<Vec<String>>(&body).expect("path list"),
            expected
        );
        let (rc, body) = dispatch_read(
            Arc::clone(&snapshot),
            "comp.props.list".into(),
            json!({"prefix": "surfaces.s"}),
        )
        .await;
        assert_eq!(rc, 0);
        assert_eq!(body.as_ref(), "[]");

        for leaf in leaves {
            let args = json!({"path": leaf.as_str()});
            assert_eq!(
                dispatch_read(Arc::clone(&snapshot), "comp.props.get".into(), args.clone())
                    .await
                    .0,
                0
            );
            assert_eq!(
                dispatch_read(Arc::clone(&snapshot), "comp.props.describe".into(), args)
                    .await
                    .0,
                0,
                "describe {}",
                leaf.as_str()
            );
        }
    }

    #[test]
    fn typed_leaf_selection_size_is_independent_of_surface_count() {
        let mut snapshot = fixture();
        let selected = snapshot
            .select(&["surfaces", "s2", "title"])
            .expect("fixture leaf exists");
        let selected_size = selected.to_string().len();
        assert!(selected.is_string());
        assert!(selected_size < 64);

        let template = snapshot
            .surfaces
            .get("s2")
            .cloned()
            .expect("fixture surface exists");
        for id in 100_u64..1_100 {
            let mut surface = template.clone();
            surface.id = id;
            snapshot.surfaces.insert(format!("s{id}"), surface);
        }

        let selected_with_many_surfaces = snapshot
            .select(&["surfaces", "s2", "title"])
            .expect("fixture leaf still exists");
        assert!(selected_with_many_surfaces.is_string());
        assert_eq!(selected_with_many_surfaces.to_string().len(), selected_size);
        assert!(snapshot.full_tree.get().is_none());
    }

    #[test]
    fn output_keys_obey_the_public_slug_encoding() {
        assert_eq!(output_key("cosmix-nested-0"), "o_cosmix_nested_0");
        assert_eq!(output_key("DP-1"), "o_dp_1");
    }

    #[test]
    fn output_slug_collision_keeps_first_and_counts_dropped_output() {
        let snapshot = fixture();
        let mut outputs = snapshot.outputs;
        let mut collisions = 0;

        assert!(output_slug_collides(
            &outputs,
            "o_dp_1",
            "DP_1",
            &mut collisions,
        ));
        assert_eq!(collisions, 1);
        assert_eq!(
            outputs
                .remove("o_dp_1")
                .expect("first output retained")
                .name,
            "DP-1"
        );
    }

    #[tokio::test]
    async fn describe_accepts_empty_collection_subtrees() {
        let mut snapshot = fixture();
        snapshot.surfaces.clear();
        snapshot.windows.clear();
        let snapshot = Arc::new(snapshot);
        for path in ["surfaces", "windows"] {
            let (rc, body) = dispatch_read(
                Arc::clone(&snapshot),
                "comp.props.describe".into(),
                json!({"path": path}),
            )
            .await;
            assert_eq!(rc, 0, "{path}: {body}");
            assert_eq!(
                serde_json::from_str::<Value>(&body)
                    .ok()
                    .and_then(|value| value.get("children").cloned()),
                Some(json!([])),
                "{path}"
            );
        }
    }

    #[tokio::test]
    async fn full_tree_serialisation_is_single_flight_and_shares_the_cached_bytes() {
        let snapshot = Arc::new(fixture());
        let left = dispatch_read(Arc::clone(&snapshot), "comp.props.get".into(), Value::Null);
        let right = dispatch_read(Arc::clone(&snapshot), "comp.props.get".into(), json!({}));
        let ((left_rc, left_body), (right_rc, right_body)) = tokio::join!(left, right);
        assert_eq!((left_rc, right_rc), (0, 0));
        assert!(Arc::ptr_eq(&left_body, &right_body));
        assert!(snapshot.full_tree.get().is_some());

        let (rc, selected) = dispatch_read(
            snapshot,
            "comp.props.get".into(),
            json!({"path": "info.service"}),
        )
        .await;
        assert_eq!(rc, 0);
        assert_eq!(selected.as_ref(), "\"comp-nested\"");
    }

    #[tokio::test]
    async fn oversized_full_tree_returns_too_large_while_leaf_read_succeeds() {
        let mut snapshot = fixture();
        let template = snapshot
            .surfaces
            .get("s2")
            .cloned()
            .expect("fixture toplevel");
        for id in 100_u64..140 {
            let mut surface = template.clone();
            surface.id = id;
            snapshot.surfaces.insert(format!("s{id}"), surface);
        }
        let snapshot = Arc::new(snapshot);
        let injected_limit = 1_024;

        let (rc, body) = dispatch_read_with_limit(
            Arc::clone(&snapshot),
            "comp.props.get".into(),
            Value::Null,
            injected_limit,
        )
        .await;
        assert_eq!(rc, 10);
        assert_eq!(
            serde_json::from_str::<Value>(&body).expect("too_large JSON"),
            json!({
                "error": "too_large",
                "limit_bytes": injected_limit,
                "hint": "read a subtree",
            })
        );
        assert!(
            snapshot
                .full_tree
                .get()
                .is_some_and(|reply| reply.bytes > injected_limit),
            "cached full-tree bytes are measured once"
        );

        let (rc, leaf) = dispatch_read_with_limit(
            snapshot,
            "comp.props.get".into(),
            json!({"path": "info.service"}),
            injected_limit,
        )
        .await;
        assert_eq!(rc, 0);
        assert_eq!(leaf.as_ref(), "\"comp-nested\"");
    }
}
