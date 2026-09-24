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
    reexports::wayland_server::Resource as _,
    wayland::shell::wlr_layer::{ExclusiveZone, KeyboardInteractivity, Layer as WlrLayer},
};

use super::dmabuf_ledger::DmabufLedgerSnapshot;
use super::presentation::SourcePresentationLeaves;
use super::presentation_stats::{OutputStats, PresentationLeaves};
use super::{
    ChromePointerGrabKind, InteractivePointer, LayerOutputBinding, LockLifecycle,
    LogicalOutputRect, SceneDecorationMode, StackBand, SurfaceId, SurfaceRecord, SurfaceRole,
    WaylandState, corner::CornerConfig, port_observation::SetValidationError, surface_stack_cmp,
};
use crate::port::ControlReply;

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
    pub(crate) occlusion: OcclusionSnapshot,
    pub(crate) info: InfoSnapshot,
    pub(crate) outputs: BTreeMap<String, OutputSnapshot>,
    pub(crate) surfaces: BTreeMap<String, SurfaceSnapshot>,
    pub(crate) windows: BTreeMap<String, WindowSnapshot>,
    pub(crate) workspaces: WorkspacesSnapshot,
    pub(crate) sources: BTreeMap<String, SourceSnapshot>,
    pub(crate) stack: Vec<u64>,
    pub(crate) focus: FocusSnapshot,
    pub(crate) decoration: DecorationSnapshot,
    pub(crate) bindings: BindingsSnapshot,
    pub(crate) input: InputSnapshot,
    #[cfg(feature = "xwayland")]
    pub(crate) xwayland: XwaylandSnapshot,
    /// Observed linux-dmabuf import outcomes (volatile; filled only in read
    /// snapshots, so the diff snapshot never sees them change).
    pub(crate) dmabuf: DmabufLedgerSnapshot,
    pub(crate) port: PortSnapshot,
    #[serde(skip)]
    full_tree: tokio::sync::OnceCell<SerialisedReply>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct OcclusionSnapshot {
    pub(crate) counters: crate::occlusion::Counters,
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
    pub(crate) explicit_sync_advertised: bool,
    pub(crate) explicit_sync_healthy: bool,
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
    /// Volatile; filled only in read snapshots (never in diffed rows).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) presentation: Option<OutputPresentationSnapshot>,
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
    #[serde(flatten)]
    pub(crate) occlusion: crate::occlusion::Props,
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
    pub(crate) fullscreen: bool,
    pub(crate) minimized: bool,
    /// The 1-based workspace of a mapped managed toplevel (X11 included —
    /// the one place an X11 window's workspace is legible, D11); null for
    /// every other role and before the first map.
    pub(crate) workspace: Option<u32>,
    pub(crate) decoration: Option<&'static str>,
    pub(crate) layer: Option<LayerSnapshot>,
    pub(crate) foreign_id: Option<String>,
    pub(crate) generation: u64,
    /// Window-only values carried to `project_window_row`; not part of the
    /// `surfaces.*` tree.
    #[serde(skip)]
    pub(crate) window: WindowExtras,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct WindowExtras {
    pub(crate) window_x: f32,
    pub(crate) window_y: f32,
    pub(crate) window_width: f32,
    pub(crate) window_height: f32,
    pub(crate) pid: Option<u64>,
    pub(crate) workspace: u32,
}

/// `workspaces.*`: the count, the default output's current workspace
/// (`current`), one `o_<slug>.current` per output (the same keys as
/// `outputs.*`) and the per-workspace window counts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct WorkspacesSnapshot {
    pub(crate) count: u32,
    pub(crate) current: u32,
    #[serde(flatten)]
    pub(crate) outputs: BTreeMap<String, OutputWorkspaceSnapshot>,
    pub(crate) list: Vec<WorkspaceRowSnapshot>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct OutputWorkspaceSnapshot {
    pub(crate) current: u32,
}

/// One `workspaces.list` entry: the 1-based index and how many mapped
/// managed toplevels are on it (X11 windows included: every row with a
/// non-null `surfaces.s<id>.workspace`, not only the `windows.*` rows).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct WorkspaceRowSnapshot {
    pub(crate) index: u32,
    pub(crate) windows: u32,
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
    #[serde(flatten)]
    pub(crate) occlusion: crate::occlusion::Props,
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
    pub(crate) fullscreen: bool,
    pub(crate) minimized: bool,
    pub(crate) output: Option<String>,
    pub(crate) band: &'static str,
    pub(crate) generation: u64,
    pub(crate) window_x: f32,
    pub(crate) window_y: f32,
    pub(crate) window_width: f32,
    pub(crate) window_height: f32,
    pub(crate) visible: bool,
    pub(crate) pid: Option<u64>,
    /// The window's 1-based workspace (writable; a move never switches).
    pub(crate) workspace: u32,
    /// Volatile; filled only in read snapshots (never in diffed rows).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) presentation: Option<PresentationLeaves>,
}

/// `outputs.o_<slug>.presentation.*` (volatile).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct OutputPresentationSnapshot {
    pub(crate) clock_id: u32,
    /// Kind flag names of the newest frame; null before the first frame.
    pub(crate) flags: Option<Vec<&'static str>>,
    pub(crate) flags_mask: Option<u32>,
    pub(crate) refresh_us: Option<u64>,
    pub(crate) frames: u64,
    pub(crate) interval_p50_us: Option<u64>,
    pub(crate) interval_p99_us: Option<u64>,
    pub(crate) since_us: u64,
}

/// `sources.<id>` (volatile, like everything a source reports).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct SourceSnapshot {
    pub(crate) output: Option<String>,
    pub(crate) registered_at_us: u64,
    pub(crate) revision: u64,
    pub(crate) registration: u64,
    pub(crate) presentation: SourcePresentationLeaves,
}

/// Select inside a small volatile object through its serialised form.
fn select_serialised<T: Serialize>(value: &T, path: &[&str]) -> Option<Value> {
    let mut node = serialise_selected(value)?;
    for segment in path {
        node = node.as_object_mut()?.remove(*segment)?;
    }
    Some(node)
}

fn serialised_node_kind<T: Serialize>(value: &T, path: &[&str]) -> Option<SnapshotNodeKind> {
    select_serialised(value, path).map(|node| {
        if node.is_object() {
            SnapshotNodeKind::Object
        } else {
            SnapshotNodeKind::Leaf
        }
    })
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct FocusSnapshot {
    pub(crate) keyboard: Option<u64>,
    pub(crate) exclusive_latch: Option<u64>,
    pub(crate) pointer: Option<u64>,
    pub(crate) pointer_grab: &'static str,
    pub(crate) session_lock: &'static str,
    pub(crate) window: FocusWindowSnapshot,
}

/// `{id, generation}` of the focused window row, both null when no window
/// has keyboard focus.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub(crate) struct FocusWindowSnapshot {
    pub(crate) id: Option<u64>,
    pub(crate) generation: Option<u64>,
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
    /// Nested backend only: whether host pointer/key input reaches the seat.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) host: Option<HostInputSnapshot>,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub(crate) struct HostInputSnapshot {
    pub(crate) passthrough: bool,
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
    /// The X display this compositor's Xwayland serves (`:N`), null until
    /// the generation is ready (XWM started, descriptor published) and
    /// again after it goes down. Read it rather than the descriptor file
    /// when the caller already speaks Bus.
    pub(crate) display: Option<Arc<str>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub(crate) struct CornersSnapshot {
    pub(crate) holders: bool,
    pub(crate) enabled: bool,
    pub(crate) deadzone_px: f64,
    pub(crate) dwell_ms: u64,
    pub(crate) velocity_max_px_s: f64,
    pub(crate) affordance: bool,
    pub(crate) discovery: bool,
    /// Volatile, read snapshots only: per edge, the panel layers comp is
    /// hiding and excluding from input itself because a conceal went
    /// unapplied past its grace (a stalled shell).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) enforced: Option<EdgeCounts>,
    /// Volatile, read snapshots only: per edge, the explicit holds
    /// (`comp.panel.hold` acquisitions) comp currently records.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) held: Option<EdgeCounts>,
}

/// One count per panel edge, summed over outputs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub(crate) struct EdgeCounts {
    pub(crate) top: u64,
    pub(crate) bottom: u64,
    pub(crate) left: u64,
    pub(crate) right: u64,
}

impl EdgeCounts {
    pub(crate) fn edge_mut(&mut self, edge: &str) -> Option<&mut u64> {
        match edge {
            "top" => Some(&mut self.top),
            "bottom" => Some(&mut self.bottom),
            "left" => Some(&mut self.left),
            "right" => Some(&mut self.right),
            _ => None,
        }
    }
}

impl From<CornerConfig> for CornersSnapshot {
    fn from(config: CornerConfig) -> Self {
        Self {
            holders: super::port_observation::HOLDER_PLANE_AVAILABLE,
            enabled: config.enabled,
            deadzone_px: config.deadzone_px,
            dwell_ms: config.dwell_ms,
            velocity_max_px_s: config.velocity_max_px_s,
            affordance: config.affordance,
            discovery: config.discovery,
            enforced: None,
            held: None,
        }
    }
}

impl CornersSnapshot {
    fn select(&self, path: &[&str]) -> Option<Value> {
        select_serialised(self, path)
    }

    fn node_kind(&self, path: &[&str]) -> Option<SnapshotNodeKind> {
        serialised_node_kind(self, path)
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
            "occlusion" => select_serialised(&self.occlusion, tail),
            "info" => self.info.select(tail),
            "outputs" => select_map(&self.outputs, tail, OutputSnapshot::select),
            "surfaces" => select_map(&self.surfaces, tail, SurfaceSnapshot::select),
            "windows" => select_map(&self.windows, tail, WindowSnapshot::select),
            "workspaces" => self.workspaces.select(tail),
            "sources" => select_map(&self.sources, tail, select_serialised::<SourceSnapshot>),
            "stack" if tail.is_empty() => serialise_selected(&self.stack),
            "focus" => self.focus.select(tail),
            "decoration" => self.decoration.select(tail),
            "bindings" => self.bindings.select(tail),
            "input" => self.input.select(tail),
            #[cfg(feature = "xwayland")]
            "xwayland" => self.xwayland.select(tail),
            "dmabuf" => self.dmabuf.select(tail),
            "port" => self.port.select(tail),
            _ => None,
        }
    }

    fn node_kind(&self, path: &[&str]) -> Option<SnapshotNodeKind> {
        let [head, tail @ ..] = path else {
            return None;
        };
        match *head {
            "occlusion" => serialised_node_kind(&self.occlusion, tail),
            "info" => self.info.node_kind(tail),
            "outputs" => map_node_kind(&self.outputs, tail, OutputSnapshot::node_kind),
            "surfaces" => map_node_kind(&self.surfaces, tail, SurfaceSnapshot::node_kind),
            "windows" => map_node_kind(&self.windows, tail, WindowSnapshot::node_kind),
            "workspaces" => self.workspaces.node_kind(tail),
            "sources" => map_node_kind(&self.sources, tail, serialised_node_kind::<SourceSnapshot>),
            "stack" if tail.is_empty() => Some(SnapshotNodeKind::Leaf),
            "focus" => self.focus.node_kind(tail),
            "decoration" => self.decoration.node_kind(tail),
            "bindings" => self.bindings.node_kind(tail),
            "input" => self.input.node_kind(tail),
            #[cfg(feature = "xwayland")]
            "xwayland" => self.xwayland.node_kind(tail),
            "dmabuf" => self.dmabuf.node_kind(tail),
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
                PatternSegment::SourceKey => {
                    append_segments(&mut paths, self.sources.keys().map(String::as_str));
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

flat_snapshot!(
    InfoSnapshot,
    service,
    version,
    backend,
    engine,
    instance,
    explicit_sync_advertised,
    explicit_sync_healthy,
);
#[cfg(feature = "xwayland")]
flat_snapshot!(XwaylandSnapshot, enabled, persist_path, display);
flat_snapshot!(DmabufLedgerSnapshot, accepted, failed, failures);
flat_snapshot!(OutputWorkspaceSnapshot, current);

impl WorkspacesSnapshot {
    fn select(&self, path: &[&str]) -> Option<Value> {
        match path {
            [] => serialise_selected(self),
            ["count"] => serialise_selected(&self.count),
            ["current"] => serialise_selected(&self.current),
            ["list"] => serialise_selected(&self.list),
            _ => select_map(&self.outputs, path, OutputWorkspaceSnapshot::select),
        }
    }

    fn node_kind(&self, path: &[&str]) -> Option<SnapshotNodeKind> {
        match path {
            [] => Some(SnapshotNodeKind::Object),
            ["count" | "current" | "list"] => Some(SnapshotNodeKind::Leaf),
            _ => map_node_kind(&self.outputs, path, OutputWorkspaceSnapshot::node_kind),
        }
    }
}

macro_rules! window_snapshot {
    ($($field:ident),+ $(,)?) => {
        impl WindowSnapshot {
            fn select(&self, path: &[&str]) -> Option<Value> {
                match path {
                    [] => serialise_selected(self),
                    $([stringify!($field)] => serialise_selected(&self.$field),)+
                    ["presentation", tail @ ..] => {
                        select_serialised(self.presentation.as_ref()?, tail)
                    }
                    _ => select_serialised(&self.occlusion, path),
                }
            }

            fn node_kind(&self, path: &[&str]) -> Option<SnapshotNodeKind> {
                match path {
                    [] => Some(SnapshotNodeKind::Object),
                    $([stringify!($field)] => Some(SnapshotNodeKind::Leaf),)+
                    ["presentation", tail @ ..] => {
                        serialised_node_kind(self.presentation.as_ref()?, tail)
                    }
                    _ => serialised_node_kind(&self.occlusion, path),
                }
            }
        }
    };
}

window_snapshot!(
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
    fullscreen,
    minimized,
    output,
    band,
    generation,
    window_x,
    window_y,
    window_width,
    window_height,
    visible,
    pid,
    workspace,
);
flat_snapshot!(FocusWindowSnapshot, id, generation);

impl FocusSnapshot {
    fn select(&self, path: &[&str]) -> Option<Value> {
        match path {
            [] => serialise_selected(self),
            ["keyboard"] => serialise_selected(&self.keyboard),
            ["exclusive_latch"] => serialise_selected(&self.exclusive_latch),
            ["pointer"] => serialise_selected(&self.pointer),
            ["pointer_grab"] => serialise_selected(&self.pointer_grab),
            ["session_lock"] => serialise_selected(&self.session_lock),
            ["window", tail @ ..] => self.window.select(tail),
            _ => None,
        }
    }

    fn node_kind(&self, path: &[&str]) -> Option<SnapshotNodeKind> {
        match path {
            [] | ["window"] => Some(SnapshotNodeKind::Object),
            ["keyboard" | "exclusive_latch" | "pointer" | "pointer_grab" | "session_lock"] => {
                Some(SnapshotNodeKind::Leaf)
            }
            ["window", tail @ ..] => self.window.node_kind(tail),
            _ => None,
        }
    }
}
flat_snapshot!(DecorationSnapshot, enabled, style);
flat_snapshot!(BindingsSnapshot, enabled, profile, table);
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
            ["presentation", tail @ ..] => select_serialised(self.presentation.as_ref()?, tail),
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
            ["presentation", tail @ ..] => serialised_node_kind(self.presentation.as_ref()?, tail),
            _ => None,
        }
    }
}

impl InputSnapshot {
    fn select(&self, path: &[&str]) -> Option<Value> {
        match path {
            [] => serialise_selected(self),
            ["corners", tail @ ..] => self.corners.select(tail),
            ["host"] => self.host.as_ref().and_then(serialise_selected),
            ["host", "passthrough"] => self
                .host
                .as_ref()
                .and_then(|host| serialise_selected(&host.passthrough)),
            _ => None,
        }
    }

    fn node_kind(&self, path: &[&str]) -> Option<SnapshotNodeKind> {
        match path {
            [] | ["corners"] => Some(SnapshotNodeKind::Object),
            ["corners", tail @ ..] => self.corners.node_kind(tail),
            ["host"] => self.host.map(|_| SnapshotNodeKind::Object),
            ["host", "passthrough"] => self.host.map(|_| SnapshotNodeKind::Leaf),
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
            ["fullscreen"] => serialise_selected(&self.fullscreen),
            ["minimized"] => serialise_selected(&self.minimized),
            ["workspace"] => serialise_selected(&self.workspace),
            ["decoration"] => serialise_selected(&self.decoration),
            ["layer"] => serialise_selected(&self.layer),
            ["layer", tail @ ..] => self.layer.as_ref()?.select(tail),
            ["foreign_id"] => serialise_selected(&self.foreign_id),
            ["generation"] => serialise_selected(&self.generation),
            _ => select_serialised(&self.occlusion, path),
        }
    }

    fn node_kind(&self, path: &[&str]) -> Option<SnapshotNodeKind> {
        match path {
            [] => Some(SnapshotNodeKind::Object),
            [
                "id" | "role" | "mapped" | "visible" | "x" | "y" | "width" | "height" | "band"
                | "sequence" | "tree_index" | "parent" | "output" | "title" | "app_id" | "focused"
                | "activated" | "maximized" | "fullscreen" | "minimized" | "workspace"
                | "decoration" | "foreign_id" | "generation",
            ] => Some(SnapshotNodeKind::Leaf),
            ["layer"] => Some(if self.layer.is_some() {
                SnapshotNodeKind::Object
            } else {
                SnapshotNodeKind::Leaf
            }),
            ["layer", tail @ ..] => self.layer.as_ref()?.node_kind(tail),
            _ => serialised_node_kind(&self.occlusion, path),
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
                presentation: None,
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
            presentation: None,
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
    // Use the same committed effective geometry that positions window_origin.
    // Without explicit xdg geometry it contains the committed surface-tree
    // bounds (including mapped subsurfaces). If no geometry is cached, the
    // origin uses zero offset and the matching extent is the full root buffer.
    let (window_width, window_height) = record
        .committed_window_geometry
        .map_or((record.layout.width, record.layout.height), |geometry| {
            (geometry.width, geometry.height)
        });
    SurfaceSnapshot {
        occlusion: crate::occlusion::Props {
            occluded: state.occlusion.is_occluded(record.id),
            occlusion_reason: state
                .occlusion
                .decisions
                .get(&record.id)
                .copied()
                .unwrap_or_default()
                .reason(),
            occlusion_revision: state
                .occlusion
                .decision_revisions
                .get(&record.id)
                .copied()
                .unwrap_or(0),
        },
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
        fullscreen: record.committed_fullscreen,
        minimized: record.minimized,
        // Stamped at the first map (rule 2): 0 until then, and only managed
        // toplevels have one. X11 toplevels have no `windows.*` row, so
        // this leaf is where their workspace is read (D11). Null again
        // once unmapped: the record keeps its last stamp until the remap
        // restamps it, but a withdrawn window is on no workspace.
        workspace: (record.mapped && record.role.managed_toplevel() && record.workspace >= 1)
            .then_some(record.workspace),
        decoration: matches!(record.role, SurfaceRole::Toplevel(_))
            .then_some(decoration_name(record.committed_decoration)),
        layer,
        foreign_id: (record.mapped && matches!(record.role, SurfaceRole::Toplevel(_)))
            .then(|| state.foreign_toplevel_identifiers.get(&record.id).cloned())
            .flatten(),
        generation: record.generation,
        window: WindowExtras {
            window_x: record.window_origin.0,
            window_y: record.window_origin.1,
            window_width,
            window_height,
            // Only rows that become windows pay for the credentials lookup.
            pid: (record.mapped && matches!(record.role, SurfaceRole::Toplevel(_)))
                .then(|| {
                    record
                        .role
                        .wl_surface()
                        .client()
                        .and_then(|client| client.get_credentials(&state.display_handle).ok())
                })
                .flatten()
                .and_then(|credentials| u64::try_from(credentials.pid).ok()),
            workspace: record.workspace,
        },
    }
}

pub(super) fn project_window_row(surface: &SurfaceSnapshot) -> WindowSnapshot {
    WindowSnapshot {
        occlusion: surface.occlusion.clone(),
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
        fullscreen: surface.fullscreen,
        minimized: surface.minimized,
        output: surface.output.clone(),
        band: surface.band,
        generation: surface.generation,
        window_x: surface.window.window_x,
        window_y: surface.window.window_y,
        window_width: surface.window.window_width,
        window_height: surface.window.window_height,
        visible: surface.visible,
        pid: surface.window.pid,
        workspace: surface.window.workspace,
        presentation: None,
    }
}

/// `workspaces.*` from the workspace state and the surface rows already
/// projected for this snapshot: per-workspace counts are the mapped
/// managed toplevels on each workspace — the rows whose `workspace` leaf
/// is non-null, so an X11 window counts although it has no `windows.*`
/// row (D11: a pager must not show a workspace empty while the mail
/// client is on it). Under a session lock they read 0, like every other
/// window-derived leaf (the `windows.*` map is empty then).
fn project_workspaces(
    state: &WaylandState,
    output_keys: &[(Output, String)],
    surfaces: &BTreeMap<String, SurfaceSnapshot>,
    session_lock_active: bool,
) -> WorkspacesSnapshot {
    let count = state.workspaces.count;
    let outputs = output_keys
        .iter()
        .map(|(_, key)| {
            (
                key.clone(),
                OutputWorkspaceSnapshot {
                    current: state.current_workspace_for(Some(key)),
                },
            )
        })
        .collect();
    let mut list = (1..=count)
        .map(|index| WorkspaceRowSnapshot { index, windows: 0 })
        .collect::<Vec<_>>();
    if !session_lock_active {
        for workspace in surfaces.values().filter_map(|row| row.workspace) {
            if let Some(slot) = workspace
                .checked_sub(1)
                .and_then(|index| list.get_mut(index as usize))
            {
                slot.windows += 1;
            }
        }
    }
    WorkspacesSnapshot {
        count,
        current: state.workspace_current(),
        outputs,
        list,
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
        window: if session_lock_active {
            FocusWindowSnapshot::default()
        } else {
            state
                .surfaces
                .values()
                .filter(|record| record.focused && record.mapped && record.role.managed_toplevel())
                .min_by_key(|record| record.id.0)
                .map_or_else(FocusWindowSnapshot::default, |record| FocusWindowSnapshot {
                    id: Some(record.id.0),
                    generation: Some(record.generation),
                })
        },
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

    let workspaces = project_workspaces(state, &output_keys, &surfaces, session_lock_active);
    let stack = project_stack(state);

    let bindings = state.bindings.port_snapshot();
    Some(CompSnapshot {
        occlusion: Default::default(),
        info: InfoSnapshot {
            service: context.service.clone(),
            version: context.version.clone(),
            backend: context.backend,
            engine: context.engine,
            instance: context.instance.clone(),
            explicit_sync_advertised: state.explicit_sync_global_advertised,
            explicit_sync_healthy: state.release_uses.explicit_sync_healthy(),
        },
        outputs,
        surfaces,
        windows,
        workspaces,
        sources: BTreeMap::new(),
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
            host: state.host_input_snapshot(),
        },
        #[cfg(feature = "xwayland")]
        xwayland: XwaylandSnapshot {
            enabled: state.xwayland.enabled,
            persist_path: Arc::from(
                super::xwayland::xwayland_enabled_persist_path(&state.xwayland.socket_name)
                    .display()
                    .to_string(),
            ),
            display: state
                .xwayland
                .display_number
                .map(|number| Arc::from(format!(":{number}"))),
        },
        dmabuf: DmabufLedgerSnapshot::default(),
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

/// Which read paths a batch of reads can reach. Volatile presentation
/// leaves are computed only for those (and never for diff baselines).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ReadScopes {
    All,
    Paths(Vec<String>),
}

impl ReadScopes {
    /// Merge one request's scope (`None` = the whole tree).
    pub(crate) fn add(&mut self, scope: Option<&str>) {
        match (&mut *self, scope) {
            (Self::All, _) => {}
            (Self::Paths(_), None) => *self = Self::All,
            (Self::Paths(paths), Some(path)) => paths.push(path.to_string()),
        }
    }

    /// Whether a read under one of the scopes can include `path`: the scope
    /// is `path`, an ancestor of it, or a descendant of it.
    pub(crate) fn wants(&self, path: &str) -> bool {
        let related = |scope: &str| {
            scope == path
                || path
                    .strip_prefix(scope)
                    .is_some_and(|rest| rest.starts_with('.'))
                || scope
                    .strip_prefix(path)
                    .is_some_and(|rest| rest.starts_with('.'))
        };
        match self {
            Self::All => true,
            Self::Paths(paths) => paths.iter().any(|scope| related(scope)),
        }
    }
}

/// A read snapshot: the diff snapshot plus the volatile presentation leaves
/// the scopes can reach.
pub(super) fn read_snapshot(
    state: &WaylandState,
    context: &SnapshotContext,
    scopes: &ReadScopes,
) -> Option<CompSnapshot> {
    let mut snapshot = snapshot(state, context)?;
    let counters = state
        .occlusion
        .bridge
        .0
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .counters;
    snapshot.occlusion.counters = counters;
    if scopes.wants("dmabuf") {
        snapshot.dmabuf = state.dmabuf_ledger.snapshot();
    }
    let (enforced, held) = super::port_observation::panel_edge_counts(state);
    snapshot.input.corners.enforced = Some(enforced);
    snapshot.input.corners.held = Some(held);
    let stats = &state.presentation.stats;
    for (key, window) in &mut snapshot.windows {
        if !scopes.wants(&format!("windows.{key}.presentation")) {
            continue;
        }
        window.presentation = Some(stats.window(window.id, window.generation).map_or_else(
            || PresentationLeaves {
                since_us: stats.epoch_us,
                ..PresentationLeaves::default()
            },
            |window| window.leaves(),
        ));
    }
    for (key, output) in &mut snapshot.outputs {
        if !scopes.wants(&format!("outputs.{key}.presentation")) {
            continue;
        }
        output.presentation = Some(output_presentation(
            stats.output(&output.name),
            stats.epoch_us,
        ));
    }
    if scopes.wants("sources") {
        snapshot.sources = state
            .presentation
            .sources
            .iter()
            .filter(|(id, _)| scopes.wants(&format!("sources.{id}")))
            .map(|(id, counters)| {
                (
                    id.clone(),
                    SourceSnapshot {
                        output: counters.output.clone(),
                        registered_at_us: counters.registered_at_us,
                        revision: counters.revision,
                        registration: counters.registration,
                        presentation: counters.leaves(),
                    },
                )
            })
            .collect();
    }
    Some(snapshot)
}

/// `wp_presentation_feedback` kind bits by name (design C3).
const PRESENTATION_FLAG_NAMES: [(u32, &str); 4] = [
    (0x1, "vsync"),
    (0x2, "hw_clock"),
    (0x4, "hw_completion"),
    (0x8, "zero_copy"),
];

pub(crate) fn presentation_flag_names(mask: u32) -> Vec<&'static str> {
    PRESENTATION_FLAG_NAMES
        .iter()
        .filter(|(bit, _)| mask & bit != 0)
        .map(|(_, name)| *name)
        .collect()
}

fn output_presentation(stats: Option<&OutputStats>, epoch_us: u64) -> OutputPresentationSnapshot {
    let intervals = stats
        .map(|stats| stats.intervals_us.summary())
        .unwrap_or_default();
    let flags_mask = stats
        .filter(|stats| stats.frames > 0)
        .map(|stats| stats.flags);
    OutputPresentationSnapshot {
        clock_id: libc::CLOCK_MONOTONIC as u32,
        flags: flags_mask.map(presentation_flag_names),
        flags_mask,
        refresh_us: stats.and_then(|stats| stats.refresh_us),
        frames: stats.map_or(0, |stats| stats.frames),
        interval_p50_us: intervals.p50,
        interval_p99_us: intervals.p99,
        since_us: stats.map_or(epoch_us, |stats| stats.since_us),
    }
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

// The slug encoding lives with the workspace model (which needs it without
// the `bus` feature); this is its published home for the port.
pub(crate) use super::workspaces::output_key;

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
    /// A content source id (`[a-z0-9_-]{1,64}`).
    SourceKey,
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
    /// Served by reads but never reported by `props.changed`.
    pub(crate) volatile: bool,
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
            volatile: false,
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

macro_rules! volatile {
    ([$($segment:expr),+ $(,)?], $ty:ident, $description:expr) => {
        DescribeEntry {
            volatile: true,
            ..descriptor!(&[$($segment),+], $ty, $description)
        }
    };
}

use PatternSegment::{Literal as L, OutputKey as O, SourceKey as C, SurfaceKey as S};

pub(crate) static DESCRIPTORS: &[DescribeEntry] = &[
    descriptor!(
        &[L("surfaces"), S, L("occluded")],
        Bool,
        "Entire canonical family is covered on every intersecting output"
    ),
    descriptor!(
        &[L("surfaces"), S, L("occlusion_reason")],
        String,
        "unknown, exposed, or opaque-coverage"
    ),
    descriptor!(
        &[L("surfaces"), S, L("occlusion_revision")],
        Number,
        "Revision of the latest visibility decision transition"
    ),
    volatile!(
        [L("occlusion"), L("counters"), L("withheld_opportunities")],
        Number,
        "Compositor-wide occlusion counter; read-only, never diffed"
    ),
    volatile!(
        [L("occlusion"), L("counters"), L("resumes")],
        Number,
        "Surface trees resumed by delivering retained callbacks; read-only, never diffed"
    ),
    volatile!(
        [L("occlusion"), L("counters"), L("recomputes")],
        Number,
        "Compositor-wide occlusion counter; read-only, never diffed"
    ),
    volatile!(
        [L("occlusion"), L("counters"), L("conservative_fallbacks")],
        Number,
        "Compositor-wide occlusion counter; read-only, never diffed"
    ),
    descriptor!(
        &[L("windows"), S, L("occluded")],
        Bool,
        "Entire canonical family is covered on every intersecting output"
    ),
    descriptor!(
        &[L("windows"), S, L("occlusion_reason")],
        String,
        "unknown, exposed, or opaque-coverage"
    ),
    descriptor!(
        &[L("windows"), S, L("occlusion_revision")],
        Number,
        "Revision of the latest visibility decision transition"
    ),
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
        &[L("info"), L("explicit_sync_advertised")],
        Bool,
        "Explicit-sync protocol global currently advertised to clients"
    ),
    descriptor!(
        &[L("info"), L("explicit_sync_healthy")],
        Bool,
        "Explicit-sync retirement pipeline has not permanently faulted"
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
        &[L("surfaces"), S, L("fullscreen")],
        Bool,
        "Committed Wayland fullscreen state"
    ),
    descriptor!(
        &[L("surfaces"), S, L("minimized")],
        Bool,
        "Compositor minimized state"
    ),
    descriptor!(
        &[L("surfaces"), S, L("workspace")],
        Number,
        "1-based workspace of a mapped managed toplevel (X11 included), else null; write windows.s<id>.workspace to move"
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
        &[L("surfaces"), S, L("generation")],
        Number,
        "Role generation; a new role (including the role ending) takes a new value, an unmap/remap of the same role keeps it"
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
        "Toplevel buffer width, CSD shadow included; window_width is the window-geometry extent",
        format = "logical_px"
    ),
    descriptor!(
        &[L("windows"), S, L("height")],
        Number,
        "Toplevel buffer height, CSD shadow included; window_height is the window-geometry extent",
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
        "Committed maximized state; writes request a configure",
        mutable
    ),
    descriptor!(
        &[L("windows"), S, L("fullscreen")],
        Bool,
        "Committed fullscreen state; writes request a configure",
        mutable
    ),
    descriptor!(
        &[L("windows"), S, L("minimized")],
        Bool,
        "Compositor minimized state; write false to restore and focus this window, true to minimise it",
        mutable
    ),
    descriptor!(
        &[L("windows"), S, L("output")],
        String,
        "Output key or null"
    ),
    // Writable (bottom|normal) and process-lifetime; the enum still lists
    // every band a read can report.
    DescribeEntry {
        mutable: true,
        persistence: Some("none"),
        ..descriptor!(&[L("windows"), S, L("band")], String, "Compositor stack band; writable as bottom|normal to demote a window behind all normal windows or restore it", enum = &["background", "bottom", "normal", "top", "overlay", "lock"])
    },
    descriptor!(
        &[L("windows"), S, L("generation")],
        Number,
        "Role generation (same value as surfaces.s<id>.generation); {id, generation} names one window"
    ),
    descriptor!(
        &[L("windows"), S, L("window_x")],
        Number,
        "Window-geometry x origin (x/y are the buffer origin, CSD shadow included); the buffer stands on a whole physical pixel, so this can be fractional at a fractional scale (1.2 at 2.5x) and is an integer at scale 1",
        format = "logical_px"
    ),
    descriptor!(
        &[L("windows"), S, L("window_y")],
        Number,
        "Window-geometry y origin; fractional at a fractional scale like window_x, an integer at scale 1",
        format = "logical_px"
    ),
    descriptor!(
        &[L("windows"), S, L("window_width")],
        Number,
        "Window-geometry width, excluding CSD shadow when the client sets geometry (width/height are the buffer extent, shadow included); without explicit geometry, uses committed surface-tree bounds like window_x/window_y, or the root buffer if no geometry is cached",
        format = "logical_px"
    ),
    descriptor!(
        &[L("windows"), S, L("window_height")],
        Number,
        "Window-geometry height, excluding CSD shadow when the client sets geometry (width/height are the buffer extent, shadow included); without explicit geometry, uses committed surface-tree bounds like window_x/window_y, or the root buffer if no geometry is cached",
        format = "logical_px"
    ),
    descriptor!(
        &[L("windows"), S, L("visible")],
        Bool,
        "Whether the window is effectively on screen (false while minimised)"
    ),
    descriptor!(
        &[L("windows"), S, L("pid")],
        Number,
        "Process id of the client socket peer (a proxy or sandbox may report its own), or null"
    ),
    descriptor!(
        &[L("windows"), S, L("workspace")],
        Number,
        "1-based workspace of this window; a write moves it there without switching (off the current workspace it reads visible:false, minimized:false)",
        mutable,
        range = "1..=count"
    ),
    descriptor!(
        &[L("workspaces"), L("count")],
        Number,
        "Number of workspaces; shrinking moves stranded windows to the last one and clamps every current",
        mutable,
        range = "1..=16"
    ),
    descriptor!(
        &[L("workspaces"), L("current")],
        Number,
        "The default output's current workspace (1-based); a write switches",
        mutable,
        range = "1..=count"
    ),
    descriptor!(
        &[L("workspaces"), O, L("current")],
        Number,
        "This output's current workspace (1-based); a write switches it (only the default output is switchable in 0.59)",
        mutable,
        range = "1..=count"
    ),
    descriptor!(
        &[L("workspaces"), L("list")],
        List,
        "One {index, windows} row per workspace; windows counts the mapped managed toplevels on it (X11 included: the surfaces.* rows with a workspace)"
    ),
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
    descriptor!(
        &[L("focus"), L("window"), L("id")],
        Number,
        "Keyboard-focused managed window (xdg or X11) id or null",
        format = "surface_id"
    ),
    descriptor!(
        &[L("focus"), L("window"), L("generation")],
        Number,
        "Role generation of the keyboard-focused window or null"
    ),
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
        &[L("input"), L("corners"), L("holders")],
        Bool,
        "Whether the panel holder control plane is available"
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
    descriptor!(
        &[L("input"), L("corners"), L("affordance")],
        Bool,
        "Whether comp draws the hotspot hover reveal, release flash and discovery flash",
        mutable
    ),
    descriptor!(
        &[L("input"), L("corners"), L("discovery")],
        Bool,
        "Whether every hotspot flashes slowly until the first corner reveal",
        mutable
    ),
    volatile!(
        [L("input"), L("corners"), L("enforced"), L("top")],
        Number,
        "Top-edge shell layers comp hides and excludes from input for an unapplied conceal; read-only, never diffed"
    ),
    volatile!(
        [L("input"), L("corners"), L("enforced"), L("bottom")],
        Number,
        "Bottom-edge shell layers comp hides and excludes from input for an unapplied conceal; read-only, never diffed"
    ),
    volatile!(
        [L("input"), L("corners"), L("enforced"), L("left")],
        Number,
        "Left-edge shell layers comp hides and excludes from input for an unapplied conceal; read-only, never diffed"
    ),
    volatile!(
        [L("input"), L("corners"), L("enforced"), L("right")],
        Number,
        "Right-edge shell layers comp hides and excludes from input for an unapplied conceal; read-only, never diffed"
    ),
    volatile!(
        [L("input"), L("corners"), L("held"), L("top")],
        Number,
        "Explicit comp.panel.hold holds recorded for top-edge panels; read-only, never diffed"
    ),
    volatile!(
        [L("input"), L("corners"), L("held"), L("bottom")],
        Number,
        "Explicit comp.panel.hold holds recorded for bottom-edge panels; read-only, never diffed"
    ),
    volatile!(
        [L("input"), L("corners"), L("held"), L("left")],
        Number,
        "Explicit comp.panel.hold holds recorded for left-edge panels; read-only, never diffed"
    ),
    volatile!(
        [L("input"), L("corners"), L("held"), L("right")],
        Number,
        "Explicit comp.panel.hold holds recorded for right-edge panels; read-only, never diffed"
    ),
    descriptor!(
        &[L("input"), L("host"), L("passthrough")],
        Bool,
        "Nested backend only: false drops host pointer and key input (resize, \
         scale and pointer leave still pass) so injected input is not overwritten",
        mutable
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
    #[cfg(feature = "xwayland")]
    descriptor!(
        &[L("xwayland"), L("display")],
        String,
        "The X display this compositor's Xwayland serves (\":N\"); null until the \
         generation is ready and again after it goes down"
    ),
    // Observed linux-dmabuf imports (TODO-comp C4): what the driver actually
    // accepted, not what it advertised. In memory only — a comp restart
    // starts from zero — and never diffed (a refusal storm would flood
    // props.changed).
    volatile!(
        [L("dmabuf"), L("accepted")],
        Number,
        "linux-dmabuf imports comp accepted since this compositor started (reset on restart)"
    ),
    volatile!(
        [L("dmabuf"), L("failed")],
        Number,
        "linux-dmabuf imports comp refused since this compositor started (reset on restart)"
    ),
    volatile!(
        [L("dmabuf"), L("failures")],
        List,
        "The newest 16 refused imports, oldest first: {format (fourcc), modifier (hex), \
         reason (invalid_metadata|descriptor_dup_failed|queue_full|worker_stopped|\
         vulkan_rejected|probe_panicked|probe_retired), detail, at_us (CLOCK_MONOTONIC)}; \
         not persisted"
    ),
    // Presentation statistics are volatile: served by get/list/describe,
    // never diffed into props.changed (a watched 60 Hz client would flood
    // the topic). Times are CLOCK_MONOTONIC µs.
    volatile!(
        [L("windows"), S, L("presentation"), L("presented")],
        Number,
        "Content updates shown since since_us (one per frame, subsurfaces included)"
    ),
    volatile!(
        [L("windows"), S, L("presentation"), L("discarded")],
        Number,
        "Content updates superseded before any frame showed them"
    ),
    volatile!(
        [L("windows"), S, L("presentation"), L("last_presented_us")],
        Number,
        "Time of the newest frame that showed an update, or null"
    ),
    volatile!(
        [L("windows"), S, L("presentation"), L("interval_p50_us")],
        Number,
        "Median interval between consecutive presentations while shown (newest 512), or null"
    ),
    volatile!(
        [L("windows"), S, L("presentation"), L("interval_p99_us")],
        Number,
        "99th percentile interval between consecutive presentations (newest 512), or null"
    ),
    volatile!(
        [L("windows"), S, L("presentation"), L("interval_max_us")],
        Number,
        "Largest interval between consecutive presentations (newest 512), or null"
    ),
    volatile!(
        [
            L("windows"),
            S,
            L("presentation"),
            L("commit_to_present_p50_us")
        ],
        Number,
        "Median buffer commit to presentation latency (newest 512), or null"
    ),
    volatile!(
        [
            L("windows"),
            S,
            L("presentation"),
            L("commit_to_present_p99_us")
        ],
        Number,
        "99th percentile buffer commit to presentation latency (newest 512), or null"
    ),
    volatile!(
        [
            L("windows"),
            S,
            L("presentation"),
            L("input_to_present_p50_us")
        ],
        Number,
        "Median injected input to first presented update committed after it, or null"
    ),
    volatile!(
        [
            L("windows"),
            S,
            L("presentation"),
            L("input_to_present_p99_us")
        ],
        Number,
        "99th percentile injected input to presentation latency, or null"
    ),
    volatile!(
        [L("windows"), S, L("presentation"), L("missed")],
        Number,
        "Vblanks skipped while an update was pending; null while the refresh is unknown (nested)"
    ),
    volatile!(
        [L("windows"), S, L("presentation"), L("refresh_us")],
        Number,
        "Fixed refresh of the newest presentation's output, or null (unknown or variable)"
    ),
    volatile!(
        [L("windows"), S, L("presentation"), L("since_us")],
        Number,
        "When counting started (the window's first update or the last reset)"
    ),
    volatile!(
        [L("outputs"), O, L("presentation"), L("clock_id")],
        Number,
        "Presentation clock id (1 = CLOCK_MONOTONIC)"
    ),
    volatile!(
        [L("outputs"), O, L("presentation"), L("flags")],
        List,
        "Kind flags of the newest frame (vsync, hw_clock, hw_completion, zero_copy), or null"
    ),
    volatile!(
        [L("outputs"), O, L("presentation"), L("flags_mask")],
        Number,
        "wp_presentation_feedback kind bits of the newest frame, or null"
    ),
    volatile!(
        [L("outputs"), O, L("presentation"), L("refresh_us")],
        Number,
        "Fixed refresh reported with the newest frame, or null (unknown or variable; never 0)"
    ),
    volatile!(
        [L("outputs"), O, L("presentation"), L("frames")],
        Number,
        "Frames presented on this output since since_us"
    ),
    volatile!(
        [L("outputs"), O, L("presentation"), L("interval_p50_us")],
        Number,
        "Median interval between presented frames (newest 512), or null"
    ),
    volatile!(
        [L("outputs"), O, L("presentation"), L("interval_p99_us")],
        Number,
        "99th percentile interval between presented frames (newest 512), or null"
    ),
    volatile!(
        [L("outputs"), O, L("presentation"), L("since_us")],
        Number,
        "When counting started (compositor start or the last reset)"
    ),
    volatile!(
        [L("sources"), C, L("output")],
        String,
        "Output the content source asked to be measured on, or null for any"
    ),
    volatile!(
        [L("sources"), C, L("registered_at_us")],
        Number,
        "When this registration of the id began"
    ),
    volatile!(
        [L("sources"), C, L("revision")],
        Number,
        "Newest content revision reported by the source"
    ),
    volatile!(
        [L("sources"), C, L("registration")],
        Number,
        "Registration number; a new one each time the id is registered"
    ),
    volatile!(
        [L("sources"), C, L("presentation"), L("presented")],
        Number,
        "Revisions shown since since_us"
    ),
    volatile!(
        [L("sources"), C, L("presentation"), L("discarded")],
        Number,
        "Revisions superseded before any frame showed them"
    ),
    volatile!(
        [L("sources"), C, L("presentation"), L("last_presented_us")],
        Number,
        "Time of the newest frame that showed a revision, or null"
    ),
    volatile!(
        [L("sources"), C, L("presentation"), L("interval_p50_us")],
        Number,
        "Median interval between consecutive presentations while shown (newest 512), or null"
    ),
    volatile!(
        [L("sources"), C, L("presentation"), L("interval_p99_us")],
        Number,
        "99th percentile interval between consecutive presentations (newest 512), or null"
    ),
    volatile!(
        [L("sources"), C, L("presentation"), L("interval_max_us")],
        Number,
        "Largest interval between consecutive presentations (newest 512), or null"
    ),
    volatile!(
        [
            L("sources"),
            C,
            L("presentation"),
            L("commit_to_present_p50_us")
        ],
        Number,
        "Median time from comp first seeing a revision to its presentation, or null"
    ),
    volatile!(
        [
            L("sources"),
            C,
            L("presentation"),
            L("commit_to_present_p99_us")
        ],
        Number,
        "99th percentile time from comp first seeing a revision to its presentation, or null"
    ),
    volatile!(
        [
            L("sources"),
            C,
            L("presentation"),
            L("input_to_present_p50_us")
        ],
        Number,
        "Median injected input to presentation of the revision that answered it, or null"
    ),
    volatile!(
        [
            L("sources"),
            C,
            L("presentation"),
            L("input_to_present_p99_us")
        ],
        Number,
        "99th percentile injected input to presentation latency, or null"
    ),
    volatile!(
        [L("sources"), C, L("presentation"), L("missed")],
        Number,
        "Vblanks skipped while a revision was pending; null while the refresh is unknown (nested)"
    ),
    volatile!(
        [L("sources"), C, L("presentation"), L("refresh_us")],
        Number,
        "Fixed refresh of the newest presentation's output, or null (unknown or variable)"
    ),
    volatile!(
        [L("sources"), C, L("presentation"), L("since_us")],
        Number,
        "When counting started (registration or the last reset)"
    ),
    volatile!(
        [L("sources"), C, L("presentation"), L("upload_bytes_total")],
        Number,
        "GPU upload bytes the source reported since since_us"
    ),
    volatile!(
        [L("sources"), C, L("presentation"), L("damage_px_total")],
        Number,
        "Damaged physical pixels the source reported since since_us"
    ),
    volatile!(
        [L("sources"), C, L("presentation"), L("upload_bytes_p50")],
        Number,
        "Median upload bytes per reported frame (newest 512), or null"
    ),
    volatile!(
        [L("sources"), C, L("presentation"), L("upload_bytes_p99")],
        Number,
        "99th percentile upload bytes per reported frame (newest 512), or null"
    ),
    volatile!(
        [L("sources"), C, L("presentation"), L("damage_px_p50")],
        Number,
        "Median damaged pixels per reported frame (newest 512), or null"
    ),
    volatile!(
        [L("sources"), C, L("presentation"), L("damage_px_p99")],
        Number,
        "99th percentile damaged pixels per reported frame (newest 512), or null"
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
                    PatternSegment::SourceKey => {
                        (1..=64).contains(&actual.len())
                            && actual.bytes().all(|byte| {
                                byte.is_ascii_lowercase()
                                    || byte.is_ascii_digit()
                                    || byte == b'_'
                                    || byte == b'-'
                            })
                    }
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
    #[serde(skip_serializing_if = "is_false")]
    volatile: bool,
}

fn slice_is_empty(values: &&[&str]) -> bool {
    values.is_empty()
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// Paths `props.changed` never reports: presentation statistics and the
/// content-source registry change every frame; the holder-plane counts
/// (`input.corners.enforced.*`, `input.corners.held.*`) are read-only
/// diagnostics served only by reads.
pub(crate) fn volatile_path(path: &str) -> bool {
    path == "sources"
        || path.starts_with("sources.")
        || path == "input.corners.enforced"
        || path.starts_with("input.corners.enforced.")
        || path == "input.corners.held"
        || path.starts_with("input.corners.held.")
        || path.split('.').any(|segment| segment == "presentation")
        || path.starts_with("occlusion.counters.")
        || path == "dmabuf"
        || path.starts_with("dmabuf.")
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
    let mut scopes = ReadScopes::Paths(Vec::new());
    for request in &state.pending_port_requests {
        scopes.add(request.scope.as_deref());
    }
    let Some(snapshot) = read_snapshot(state, &context, &scopes).map(Arc::new) else {
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
    if command == "comp.windows.list" {
        return enforce_reply_limit(windows_list(&snapshot, &args), limit_bytes);
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

fn list_argument(path: &str, expected: &'static str, range: &'static str) -> (u8, Arc<str>) {
    ControlReply::Validation(SetValidationError::InvalidValue {
        path: path.into(),
        expected,
        range,
    })
    .into_wire()
}

/// `comp.windows.list {app_id?, title?, title_contains?, visible?,
/// workspace?}`: the window rows matching every given filter, in id order.
/// `workspace` is an index, `"current"` (this snapshot's current workspace)
/// or `"all"` (the default: no filter, so 0.58 callers see the same set).
fn windows_list(snapshot: &CompSnapshot, args: &Value) -> (u8, Arc<str>) {
    const ALLOWED: &[&str] = &["app_id", "title", "title_contains", "visible", "workspace"];
    let empty = serde_json::Map::new();
    let object = match args {
        Value::Null => &empty,
        Value::Object(object) => object,
        _ => return list_argument("args", "JSON object", "filter object"),
    };
    if let Some(field) = object
        .keys()
        .find(|field| !ALLOWED.contains(&field.as_str()))
    {
        return ControlReply::InvalidArgs {
            field: field.clone(),
            allowed: ALLOWED,
        }
        .into_wire();
    }
    let mut texts = [None; 3];
    for (slot, name) in texts.iter_mut().zip(["app_id", "title", "title_contains"]) {
        match object.get(name) {
            None | Some(Value::Null) => {}
            Some(Value::String(value)) if value.len() <= 4096 => *slot = Some(value.as_str()),
            Some(_) => return list_argument(name, "string", "at most 4096 bytes"),
        }
    }
    let [app_id, title, title_contains] = texts;
    let visible = match object.get("visible") {
        None | Some(Value::Null) => None,
        Some(Value::Bool(visible)) => Some(*visible),
        Some(_) => return list_argument("visible", "bool", "true|false"),
    };
    // Rule 4: `"current"` is resolved against this snapshot's current
    // workspace, so the reply is consistent with the rows it lists. An
    // index above the count is refused like every other workspace input
    // (props.set, the ingress gate): "no such workspace", not "no windows
    // there".
    const WORKSPACE_RANGE: &str = "1..=count|current|all";
    let workspace = match object.get("workspace") {
        None | Some(Value::Null) => None,
        Some(Value::String(word)) if word == "all" => None,
        Some(Value::String(word)) if word == "current" => Some(snapshot.workspaces.current),
        Some(Value::Number(number)) => match number
            .as_u64()
            .and_then(|index| u32::try_from(index).ok())
        {
            Some(index) if (1..=snapshot.workspaces.count).contains(&index) => Some(index),
            _ => {
                return list_argument("workspace", "unsigned integer or string", WORKSPACE_RANGE);
            }
        },
        Some(_) => {
            return list_argument("workspace", "unsigned integer or string", WORKSPACE_RANGE);
        }
    };
    let mut rows = snapshot
        .windows
        .values()
        .filter(|row| {
            app_id.is_none_or(|app_id| row.app_id.as_deref() == Some(app_id))
                && title.is_none_or(|title| row.title.as_deref() == Some(title))
                && title_contains.is_none_or(|needle| {
                    row.title
                        .as_deref()
                        .is_some_and(|title| title.contains(needle))
                })
                && visible.is_none_or(|visible| row.visible == visible)
                && workspace.is_none_or(|workspace| row.workspace == workspace)
        })
        .collect::<Vec<_>>();
    rows.sort_by_key(|row| row.id);
    match serde_json::to_string(&json!({ "windows": rows })) {
        Ok(body) => (0, Arc::from(body)),
        Err(_) => error("busy"),
    }
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
            volatile: entry.volatile,
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
        volatile: volatile_path(path.as_str()),
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
                presentation: Some(OutputPresentationSnapshot {
                    clock_id: 1,
                    flags: Some(vec!["vsync"]),
                    flags_mask: Some(1),
                    refresh_us: None,
                    frames: 3,
                    interval_p50_us: Some(16_000),
                    interval_p99_us: Some(17_000),
                    since_us: 5,
                }),
            },
        );
        let layer = SurfaceSnapshot {
            occlusion: Default::default(),
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
            fullscreen: false,
            minimized: false,
            workspace: None,
            decoration: None,
            layer: Some(LayerSnapshot {
                stratum: "top",
                interactivity: "exclusive",
                exclusive_zone: 30,
                binding: "explicit",
            }),
            foreign_id: None,
            generation: 3,
            window: WindowExtras::default(),
        };
        let toplevel = SurfaceSnapshot {
            occlusion: Default::default(),
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
            fullscreen: false,
            minimized: false,
            workspace: Some(1),
            decoration: Some("server"),
            layer: None,
            foreign_id: Some("foreign-2".into()),
            generation: 4,
            window: WindowExtras {
                window_x: 52.0,
                window_y: 72.0,
                window_width: 776.0,
                window_height: 576.0,
                pid: Some(4242),
                workspace: 1,
            },
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
        popup.workspace = None;
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
        subsurface.workspace = None;
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
        lock.workspace = None;
        surfaces.insert("s5".into(), lock);
        let mut windows = BTreeMap::new();
        windows.insert(
            "s2".into(),
            WindowSnapshot {
                occlusion: Default::default(),
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
                fullscreen: toplevel.fullscreen,
                minimized: toplevel.minimized,
                output: toplevel.output.clone(),
                band: toplevel.band,
                generation: toplevel.generation,
                window_x: toplevel.window.window_x,
                window_y: toplevel.window.window_y,
                window_width: toplevel.window.window_width,
                window_height: toplevel.window.window_height,
                visible: toplevel.visible,
                pid: toplevel.window.pid,
                workspace: toplevel.window.workspace,
                presentation: Some(PresentationLeaves {
                    presented: 3,
                    interval_p50_us: Some(16_000),
                    since_us: 5,
                    ..PresentationLeaves::default()
                }),
            },
        );
        let mut workspace_outputs = BTreeMap::new();
        workspace_outputs.insert(output.clone(), OutputWorkspaceSnapshot { current: 1 });
        let workspaces = WorkspacesSnapshot {
            count: 2,
            current: 1,
            outputs: workspace_outputs,
            list: vec![
                WorkspaceRowSnapshot {
                    index: 1,
                    windows: 1,
                },
                WorkspaceRowSnapshot {
                    index: 2,
                    windows: 0,
                },
            ],
        };
        let mut sources = BTreeMap::new();
        sources.insert(
            "scene".to_string(),
            SourceSnapshot {
                output: None,
                registered_at_us: 7,
                revision: 4,
                registration: 1,
                presentation: SourcePresentationLeaves {
                    upload_bytes_total: 640,
                    ..SourcePresentationLeaves::default()
                },
            },
        );
        CompSnapshot {
            occlusion: Default::default(),
            info: InfoSnapshot {
                service: Arc::from("comp-nested"),
                version: Arc::from("0.37.0"),
                backend: "nested",
                engine: "bevy-0.19/wgpu",
                instance: Arc::from("fixture"),
                explicit_sync_advertised: false,
                explicit_sync_healthy: true,
            },
            outputs,
            surfaces,
            windows,
            workspaces,
            sources,
            stack: vec![1, 2],
            focus: FocusSnapshot {
                keyboard: Some(2),
                exclusive_latch: Some(1),
                pointer: Some(2),
                pointer_grab: "none",
                session_lock: "none",
                window: FocusWindowSnapshot {
                    id: Some(2),
                    generation: Some(4),
                },
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
                // A read snapshot, with the volatile holder-plane counts.
                corners: CornersSnapshot {
                    enforced: Some(EdgeCounts { left: 1, ..EdgeCounts::default() }),
                    held: Some(EdgeCounts::default()),
                    ..CornersSnapshot::from(CornerConfig::default())
                },
                host: Some(HostInputSnapshot { passthrough: true }),
            },
            #[cfg(feature = "xwayland")]
            xwayland: XwaylandSnapshot {
                enabled: true,
                persist_path: Arc::from("/tmp/fixture/etc/comp/xwayland-enabled.comp-nested"),
                display: Some(Arc::from(":3")),
            },
            // A read snapshot, with the volatile import ledger.
            dmabuf: DmabufLedgerSnapshot {
                accepted: 5,
                failed: 1,
                failures: vec![super::super::dmabuf_ledger::DmabufFailureRecord {
                    format: "AR24".into(),
                    modifier: "0x0000000000000000".into(),
                    reason: "vulkan_rejected",
                    detail: "fixture".into(),
                    at_us: 9,
                }],
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
    fn corner_hold_property_is_absent_from_snapshot_and_schema() {
        let snapshot = fixture();
        assert_eq!(snapshot.select(&["input", "corners", "hold_ms"]), None);
        assert_eq!(snapshot.node_kind(&["input", "corners", "hold_ms"]), None);
        let corners = snapshot.select(&["input", "corners"]).unwrap();
        assert!(corners.get("hold_ms").is_none());
        assert!(describe(&snapshot, &PropPath::new("input.corners.hold_ms").unwrap()).is_none());
    }

    #[test]
    fn mutable_descriptors_match_the_writable_leaves() {
        let snapshot = fixture();
        let mutable = DESCRIPTORS
            .iter()
            .filter(|descriptor| descriptor.mutable)
            .collect::<Vec<_>>();
        // The corner leaves and the window band are process-lifetime
        // (persistence "none"); `xwayland.enabled` is deliberately the
        // surface's ONE file-persisted mutable leaf (startup-read — a
        // non-persisted startup switch would be unreachable from its own
        // surface).
        // 0.59.0 adds the four workspace leaves: the window's workspace,
        // the count, and the current workspace by default output and by
        // output key.
        // Chunk 19 adds the two affordance leaves, `input.corners.affordance`
        // and `input.corners.discovery`.
        #[cfg(feature = "xwayland")]
        assert_eq!(mutable.len(), 16);
        #[cfg(not(feature = "xwayland"))]
        assert_eq!(mutable.len(), 15);
        for path in [
            "input.corners.enabled",
            "input.corners.deadzone_px",
            "input.corners.dwell_ms",
            "input.corners.velocity_max_px_s",
            "input.corners.affordance",
            "input.corners.discovery",
            "input.host.passthrough",
            "windows.s2.band",
            "windows.s2.minimized",
            "windows.s2.maximized",
            "windows.s2.fullscreen",
            "windows.s2.workspace",
            "workspaces.count",
            "workspaces.current",
            "workspaces.o_dp_1.current",
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
        // The workspace rows and per-output object are read-only; the
        // per-output current is a leaf under an object keyed like outputs.
        let list = describe(&snapshot, &PropPath::new("workspaces.list").unwrap()).unwrap();
        let list = serde_json::from_str::<Value>(&list).unwrap();
        assert_eq!(list["mutable"], false);
        assert_eq!(list["type"], "list");
        assert_eq!(
            snapshot.select(&["workspaces", "list"]),
            Some(json!([{"index": 1, "windows": 1}, {"index": 2, "windows": 0}]))
        );
        assert_eq!(
            snapshot.select(&["workspaces", "o_dp_1", "current"]),
            Some(json!(1))
        );
        assert_eq!(
            snapshot.node_kind(&["workspaces", "o_dp_1"]),
            Some(SnapshotNodeKind::Object)
        );
        assert_eq!(snapshot.node_kind(&["workspaces", "o_nope"]), None);
        assert_eq!(
            snapshot.select(&["surfaces", "s3", "workspace"]),
            Some(Value::Null)
        );
        assert_eq!(
            snapshot.select(&["windows", "s2", "workspace"]),
            Some(json!(1))
        );
    }

    #[test]
    fn capability_leaf_reflects_holder_plane() {
        let snapshot = fixture();
        assert_eq!(snapshot.select(&["input", "corners", "holders"]),
            Some(json!(super::super::port_observation::HOLDER_PLANE_AVAILABLE)));
        // Quoin goes command-driven on this leaf: it is true only because
        // holder tracking, the conceal timer, enforcement on a stalled shell,
        // disconnect cleanup and resynchronisation all exist (chunk 15).
        assert_eq!(snapshot.select(&["input", "corners", "holders"]), Some(json!(true)));
        // The enforcement and hold counts are read-only, volatile leaves.
        assert_eq!(snapshot.select(&["input", "corners", "enforced", "left"]), Some(json!(1)));
        for path in ["input.corners.enforced.left", "input.corners.held.top"] {
            let descriptor: Value = serde_json::from_str(
                &describe(&snapshot, &PropPath::new(path).unwrap()).unwrap(),
            )
            .unwrap();
            assert_eq!(descriptor["mutable"], false, "{path}");
            assert_eq!(descriptor["volatile"], true, "{path}");
            assert!(matches!(
                super::super::port_observation::validate_set_request(path, &json!(0)),
                Err(super::super::port_observation::SetValidationError::ReadOnly)
            ), "{path}");
        }
        let path = PropPath::new("input.corners.holders").unwrap();
        let descriptor: Value = serde_json::from_str(&describe(&snapshot, &path).unwrap()).unwrap();
        assert_eq!(descriptor["mutable"], false);
        assert_eq!(descriptor["type"], "bool");
        assert!(matches!(super::super::port_observation::validate_set_request(
            "input.corners.holders", &json!(false)),
            Err(super::super::port_observation::SetValidationError::ReadOnly)));
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

    /// S14: the descriptor table's `volatile` flag and the path rule
    /// `props.changed` filters on never disagree.
    #[test]
    fn descriptor_volatility_matches_the_path_rule() {
        let snapshot = fixture();
        assert_eq!(
            snapshot.select(&["occlusion", "counters", "resumes"]),
            Some(serde_json::json!(0))
        );
        assert!(
            snapshot
                .select(&["surfaces", "s1", "occlusion_counters"])
                .is_none()
        );
        let mut volatile = 0;
        for descriptor in DESCRIPTORS {
            let path = descriptor
                .pattern
                .iter()
                .map(|segment| match segment {
                    PatternSegment::Literal(literal) => *literal,
                    PatternSegment::OutputKey => "o_dp_1",
                    PatternSegment::SurfaceKey => "s2",
                    PatternSegment::SourceKey => "scene",
                })
                .collect::<Vec<_>>()
                .join(".");
            assert_eq!(descriptor.volatile, volatile_path(&path), "{path}");
            volatile += usize::from(descriptor.volatile);
        }
        // + 8: the four `input.corners.enforced.*` and four `held.*` counts.
        // + 3: the `dmabuf.*` import ledger.
        assert_eq!(volatile, 13 + 8 + 4 + 19 + 4 + 8 + 3);
    }

    #[test]
    fn scopes_reach_ancestors_and_descendants_only() {
        let scopes = ReadScopes::Paths(vec!["windows.s2".into(), "outputs".into()]);
        assert!(scopes.wants("windows.s2.presentation"));
        assert!(scopes.wants("outputs.o_dp_1.presentation"));
        assert!(!scopes.wants("windows.s20.presentation"));
        assert!(!scopes.wants("sources"));
        assert!(ReadScopes::Paths(vec!["sources.scene.revision".into()]).wants("sources"));
        let mut merged = ReadScopes::Paths(Vec::new());
        assert!(!merged.wants("sources"));
        merged.add(Some("info"));
        assert!(!merged.wants("sources"));
        merged.add(None);
        assert_eq!(merged, ReadScopes::All);
    }

    #[test]
    fn flag_names_follow_the_kind_bits() {
        assert_eq!(presentation_flag_names(0), Vec::<&str>::new());
        assert_eq!(
            presentation_flag_names(0x7),
            ["vsync", "hw_clock", "hw_completion"]
        );
        assert_eq!(presentation_flag_names(0x8), ["zero_copy"]);
    }

    #[test]
    fn dmabuf_import_ledger_is_served_read_only_and_volatile() {
        let snapshot = fixture();
        let describe_json = |path: &str| {
            let body = describe(&snapshot, &PropPath::new(path).unwrap())
                .unwrap_or_else(|| panic!("describe {path}"));
            serde_json::from_str::<Value>(&body).unwrap()
        };
        for path in ["dmabuf.accepted", "dmabuf.failed", "dmabuf.failures"] {
            let body = describe_json(path);
            assert_eq!(body["volatile"], true, "{path}");
            assert_eq!(body["mutable"], false, "{path}");
            assert!(volatile_path(path), "{path}");
        }
        assert_eq!(describe_json("dmabuf.failures")["type"], "list");
        assert_eq!(snapshot.select(&["dmabuf", "accepted"]), Some(json!(5)));
        assert_eq!(snapshot.select(&["dmabuf", "failed"]), Some(json!(1)));
        assert_eq!(
            snapshot.select(&["dmabuf", "failures"]),
            Some(json!([{
                "format": "AR24",
                "modifier": "0x0000000000000000",
                "reason": "vulkan_rejected",
                "detail": "fixture",
                "at_us": 9
            }]))
        );
        let leaves = snapshot
            .leaf_paths()
            .into_iter()
            .map(|path| path.as_str().to_owned())
            .collect::<Vec<_>>();
        for path in ["dmabuf.accepted", "dmabuf.failed", "dmabuf.failures"] {
            assert!(leaves.iter().any(|leaf| leaf == path), "{path} listed");
        }
    }

    #[test]
    fn presentation_and_source_leaves_are_described_as_volatile() {
        let snapshot = fixture();
        let describe_json = |path: &str| {
            let body = describe(&snapshot, &PropPath::new(path).unwrap())
                .unwrap_or_else(|| panic!("describe {path}"));
            serde_json::from_str::<Value>(&body).unwrap()
        };
        for path in [
            "windows.s2.presentation.presented",
            "windows.s2.presentation.missed",
            "outputs.o_dp_1.presentation.frames",
            "sources.scene.revision",
            "sources.scene.output",
            "sources.scene.presentation.upload_bytes_p99",
        ] {
            let body = describe_json(path);
            assert_eq!(body["volatile"], true, "{path}");
            assert_eq!(body["mutable"], false, "{path}");
            assert!(volatile_path(path), "{path}");
        }
        for path in ["windows.s2.presentation", "sources.scene", "sources"] {
            let body = describe_json(path);
            assert_eq!(body["type"], "object");
            assert_eq!(body["volatile"], true, "{path}");
        }
        assert!(
            describe_json("windows.s2.presentation")["children"]
                .as_array()
                .unwrap()
                .contains(&json!("windows.s2.presentation.since_us"))
        );
        for path in ["windows.s2.title", "outputs.o_dp_1", "windows"] {
            assert!(describe_json(path).get("volatile").is_none(), "{path}");
            assert!(!volatile_path(path), "{path}");
        }
        assert_eq!(
            snapshot.select(&["sources", "scene", "presentation", "upload_bytes_total"]),
            Some(json!(640))
        );
        assert_eq!(
            snapshot.select(&["windows", "s2", "presentation", "missed"]),
            Some(Value::Null),
            "unmeasured missed is null"
        );
        assert_eq!(
            snapshot.select(&["outputs", "o_dp_1", "presentation", "clock_id"]),
            Some(json!(1))
        );
        assert_eq!(snapshot.select(&["sources", "scene", "nope"]), None);
        assert_eq!(
            snapshot.select(&["windows", "s2", "presentation", "presented", "x"]),
            None
        );
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
