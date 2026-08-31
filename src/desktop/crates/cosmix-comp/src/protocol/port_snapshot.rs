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
    reexports::wayland_server::Resource,
    wayland::shell::wlr_layer::{ExclusiveZone, KeyboardInteractivity, Layer as WlrLayer},
};

use super::{
    ChromePointerGrabKind, InteractivePointer, LayerOutputBinding, SceneDecorationMode, StackBand,
    SurfaceId, SurfaceRecord, SurfaceRole, WaylandState, surface_stack_cmp,
};

pub(crate) const BROKER_RETRYING: u8 = 0;
pub(crate) const BROKER_CONNECTED: u8 = 1;

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
    pub(crate) port: PortSnapshot,
    #[serde(skip)]
    property_tree: OnceLock<Value>,
    #[serde(skip)]
    full_tree: tokio::sync::OnceCell<Arc<str>>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct InfoSnapshot {
    pub(crate) service: Arc<str>,
    pub(crate) version: Arc<str>,
    pub(crate) backend: &'static str,
    pub(crate) engine: &'static str,
    pub(crate) instance: Arc<str>,
}

#[derive(Clone, Debug, Serialize)]
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

#[derive(Clone, Copy, Debug, Serialize)]
pub(crate) struct RectSnapshot {
    pub(crate) x: f32,
    pub(crate) y: f32,
    pub(crate) width: f32,
    pub(crate) height: f32,
}

#[derive(Clone, Debug, Serialize)]
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

#[derive(Clone, Debug, Serialize)]
pub(crate) struct LayerSnapshot {
    pub(crate) stratum: &'static str,
    pub(crate) interactivity: &'static str,
    pub(crate) exclusive_zone: i32,
    pub(crate) binding: &'static str,
}

#[derive(Clone, Debug, Serialize)]
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
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct FocusSnapshot {
    pub(crate) keyboard: Option<u64>,
    pub(crate) exclusive_latch: Option<u64>,
    pub(crate) pointer: Option<u64>,
    pub(crate) pointer_grab: &'static str,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct DecorationSnapshot {
    pub(crate) enabled: bool,
    pub(crate) style: &'static str,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct BindingsSnapshot {
    pub(crate) enabled: bool,
    pub(crate) profile: &'static str,
    pub(crate) table: Vec<BindingRowSnapshot>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct BindingRowSnapshot {
    pub(crate) chord: String,
    pub(crate) action: &'static str,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct PortSnapshot {
    pub(crate) level: &'static str,
    pub(crate) event_seq: u64,
    pub(crate) lost_count: u64,
    pub(crate) queue_depth: usize,
    pub(crate) reply_timeouts: u64,
    pub(crate) slug_collisions: u64,
    pub(crate) broker: &'static str,
}

/// Build one fully owned snapshot on the protocol thread.
pub(super) fn snapshot(state: &WaylandState, context: &SnapshotContext) -> Option<CompSnapshot> {
    let sources = state.backend.port_outputs();
    let mut output_keys = Vec::<(Output, String)>::with_capacity(sources.len());
    let mut outputs = BTreeMap::<String, OutputSnapshot>::new();
    let mut slug_collisions = 0_u64;
    for source in sources {
        let key = output_key(&source.name);
        if output_slug_collides(&outputs, &key, &source.name, &mut slug_collisions) {
            continue;
        }
        let usable = state.port_usable_output_rect_for(&source.output)?;
        output_keys.push((source.output, key.clone()));
        outputs.insert(
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

    let default_output = state.backend.default_output();
    let mut surfaces = BTreeMap::new();
    for record in state
        .surfaces
        .values()
        .filter(|record| !matches!(record.role, SurfaceRole::Dormant(_)))
    {
        let output = surface_output(state, record, default_output.as_ref())
            .and_then(|output| output_key_for(&output_keys, output));
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
        let key = surface_key(record.id);
        surfaces.insert(
            key,
            SurfaceSnapshot {
                id: record.id.0,
                role: record.role.kind(),
                mapped: record.mapped,
                visible: record.layout.visible,
                x: record.layout.x,
                y: record.layout.y,
                width: record.layout.width,
                height: record.layout.height,
                band: band_name(record.layout.z.band),
                sequence: record.layout.z.sequence,
                tree_index: record.layout.z.tree_index,
                parent: record.layout.parent.map(|id| id.0),
                output,
                title: record.title.clone(),
                app_id: record.app_id.clone(),
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
            },
        );
    }

    let windows = surfaces
        .iter()
        .filter(|(_, surface)| surface.role == "toplevel" && surface.mapped)
        .map(|(key, surface)| {
            (
                key.clone(),
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
                },
            )
        })
        .collect();

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
    let stack = roots.into_iter().map(|record| record.id.0).collect();

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
        focus: FocusSnapshot {
            keyboard: state
                .keyboard
                .current_focus()
                .as_ref()
                .and_then(|surface| state.surfaces.get(&surface.id()))
                .map(|record| record.id.0),
            exclusive_latch: state
                .exclusive_keyboard_focus
                .as_ref()
                .and_then(|object| state.surfaces.get(object))
                .map(|record| record.id.0),
            pointer: state
                .pointer
                .current_focus()
                .as_ref()
                .and_then(|surface| state.surfaces.get(&surface.id()))
                .map(|record| record.id.0),
            pointer_grab: pointer_grab_name(state),
        },
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
        port: PortSnapshot {
            level: "L1",
            event_seq: 0,
            lost_count: 0,
            queue_depth: context.queue_depth.load(Ordering::Acquire),
            reply_timeouts: context.reply_timeouts.load(Ordering::Acquire),
            slug_collisions,
            broker: if context.broker.load(Ordering::Acquire) == BROKER_CONNECTED {
                "connected"
            } else {
                "retrying"
            },
        },
        property_tree: OnceLock::new(),
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
    static WARNED_KEYS: OnceLock<std::sync::Mutex<BTreeSet<String>>> = OnceLock::new();
    let mut warned = WARNED_KEYS
        .get_or_init(|| std::sync::Mutex::new(BTreeSet::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if warned.insert(key.to_string()) {
        tracing::warn!(
            slug = key,
            kept_output = %first.name,
            dropped_output,
            "compositor Bus output slug collision; keeping first output"
        );
    }
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
        SurfaceRole::Popup(_) | SurfaceRole::Subsurface { .. } | SurfaceRole::Dormant(_) => None,
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
            owner: "comp",
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
    descriptor!(&[L("port"), L("level")], String, "Implemented property substrate level", enum = &["L1"]),
    descriptor!(
        &[L("port"), L("event_seq")],
        Number,
        "Property event sequence; zero until P-1"
    ),
    descriptor!(
        &[L("port"), L("lost_count")],
        Number,
        "Lost property events; zero until P-1"
    ),
    descriptor!(
        &[L("port"), L("queue_depth")],
        Number,
        "Accepted snapshot requests not yet completed"
    ),
    descriptor!(
        &[L("port"), L("reply_timeouts")],
        Number,
        "Replies dropped after a send deadline or saturated reply lane"
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
    if command == "comp.info" {
        return (
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
                return full_tree(snapshot)
                    .await
                    .map_or_else(|()| error("busy"), |body| (0, body));
            }
            Ok(Some(_)) => {}
            Err(()) => return error("unknown_path"),
        }
    }
    tokio::task::spawn_blocking(move || dispatch_selected_read(&snapshot, &command, &args))
        .await
        .unwrap_or_else(|_| error("busy"))
}

async fn full_tree(snapshot: Arc<CompSnapshot>) -> Result<Arc<str>, ()> {
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
                    .map(Arc::<str>::from)
                    .map_err(|_| ())
            })
            .await
            .map_err(|_| ())??;
            Ok(body)
        })
        .await
        .map(Arc::clone)
}

fn dispatch_selected_read(snapshot: &CompSnapshot, command: &str, args: &Value) -> (u8, Arc<str>) {
    let tree = match property_tree(snapshot) {
        Ok(tree) => tree,
        Err(()) => return error("busy"),
    };
    match command {
        "comp.props.get" => match optional_path(args, "path") {
            Ok(Some(path)) => select(tree, &path).map_or_else(
                || error("unknown_path"),
                |value| (0, Arc::from(value.to_string())),
            ),
            Ok(None) => error("busy"),
            Err(()) => error("unknown_path"),
        },
        "comp.props.list" => match optional_path(args, "prefix") {
            Ok(prefix) => {
                let leaves = flattened_paths(tree);
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
            Ok(path) => describe(tree, &path)
                .map_or_else(|| error("unknown_path"), |body| (0, Arc::from(body))),
            Err(()) => error("unknown_path"),
        },
        _ => error("unknown_verb"),
    }
}

fn property_tree(snapshot: &CompSnapshot) -> Result<&Value, ()> {
    if snapshot.property_tree.get().is_none() {
        let tree = serde_json::to_value(snapshot).map_err(|_| ())?;
        let _ = snapshot.property_tree.set(tree);
    }
    snapshot.property_tree.get().ok_or(())
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

fn select<'a>(tree: &'a Value, path: &PropPath) -> Option<&'a Value> {
    let mut current = tree;
    for segment in path.segments() {
        current = current.as_object()?.get(segment)?;
    }
    Some(current)
}

pub(crate) fn flattened_paths(tree: &Value) -> Vec<PropPath> {
    let mut paths = Vec::new();
    flatten_into(tree, "", &mut paths);
    paths
}

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

fn describe(tree: &Value, path: &PropPath) -> Option<String> {
    let value = select(tree, path)?;
    let matches = DESCRIPTORS
        .iter()
        .copied()
        .filter(|entry| entry.matches(path))
        .collect::<Vec<_>>();
    if !value.is_object() {
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
            owner: entry.owner,
            children: None,
        })
        .ok();
    }

    let leaves = flattened_paths(tree);
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
            },
        );
        CompSnapshot {
            info: InfoSnapshot {
                service: Arc::from("comp-nested"),
                version: Arc::from("0.33.0"),
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
            port: PortSnapshot {
                level: "L1",
                event_seq: 0,
                lost_count: 0,
                queue_depth: 1,
                reply_timeouts: 0,
                slug_collisions: 0,
                broker: "connected",
            },
            property_tree: OnceLock::new(),
            full_tree: tokio::sync::OnceCell::new(),
        }
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
}
