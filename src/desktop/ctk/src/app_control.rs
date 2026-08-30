//! The outward Bus app-control surface (`ctk-app-control.v0`, draft).
//!
//! CTK apps decline the Bus Display Protocol renderer but keep the ARexx
//! model's other half: an addressable command port. [`AppPortPlugin`] owns the
//! one inbound drain + response path for every registered port request another
//! mesh citizen directs at the app's control identity
//! ([`BusBridgeConfig::service_name`]). Exact named verbs register typed Bevy
//! systems with the port; each handler gets its own system params and returns
//! one JSON [`AppPortReply`]. This is dispatch plumbing, not a generic
//! stringly-typed catch-all wire contract. The generic action contract is the
//! registered, schema-checked surface in [`crate::action_control`].
//!
//! * `app.describe` — contract id, app identity, title, control count.
//! * `app.controls.list` — metadata for every registered control (no values).
//! * `app.controls.get` (`target=`) — the live value of one control.
//! * `app.controls.set` (`target=`, `value=`) — drive one control **as
//!   the user would**: the value is canonicalised, the view updates via the
//!   non-emitting programmatic events, and a final [`ControlChange`] flows
//!   through the app's ordinary write pipeline (for the mixer: the same
//!   optimistic revisioned write a mouse release commits). Action buttons
//!   receive [`Activate`]. RC 0 means *validated and locally dispatched*, not
//!   that a downstream daemon has applied it.
//!
//! Arguments are accepted from either surface a caller naturally has: an Bus
//! header (`target=…` on the wire — the display protocol's convention) or a
//! field of a JSON body object (Mix's `send svc cmd target="…"` serialises
//! its k=v pairs into the body). Headers win on conflict. The control is
//! addressed by `target`, NOT `id`: on the wire `id` is the request
//! correlation header, owned by the transport — a control caller's `id=` pair
//! can never reach the app. `action.invoke` therefore carries its action `id`
//! inside the JSON/`args` object, never as the Bus correlation header.
//!
//! Values are **domain** values (dB, bools, seconds), not normalised widget
//! positions. Replies are JSON bodies; errors are RC 10 with
//! `{"error": "..."}` (the worker answers RC 11 busy before requests reach
//! this module). Read-only discovery (`app.describe`, `app.controls.list`/
//! `.get`, `actions.list`/`.describe`) retains its established open contract.
//! **Every other app-port verb** — named app verbs included — applies the same
//! fail-closed local caller gate as `action.invoke` and `app.controls.set`:
//! the recipient noded must stamp `broker_origin: local`, and the caller must
//! also have a canonical registered service identity. Named verbs can mutate
//! app state (quit, transport, loads), so they are never anonymous.
//!
//! [`WidgetControlPlugin`] supplies the three `app.controls.*` verbs. The
//! compatibility [`AppControlPlugin`] composes the base port + widget-control
//! plugin, so existing callers keep the same surface. Apps that want only
//! app-local named verbs install [`AppPortPlugin`] without widget control.
//!
//! The control registry keys on [`BusWidget::id`] — unique per app by construction
//! (widget constructors require an id; duplicates are rejected here with a
//! warning). Widgets that are neither queryable nor writable (layout spacers)
//! never register.

use std::collections::HashMap;

use bevy::app::{App, Plugin, Update};
use bevy::ecs::entity::Entity;
use bevy::ecs::lifecycle::RemovedComponents;
use bevy::ecs::query::{Added, Has};
use bevy::ecs::resource::Resource;
use bevy::ecs::schedule::IntoScheduleConfigs;
use bevy::ecs::system::{Commands, In, IntoSystem, Query, Res, ResMut, SystemId};
use bevy::ecs::world::World;
use bevy::log::warn;
use bevy::ui::{Checked, InteractionDisabled};
use bevy::ui_widgets::{Activate, SliderDragState};
use serde_json::json;

use crate::bus::{BusBridge, InboundRequest};
use crate::widgets::{
    ActionButton, ActiveControlGesture, BusWidget, ControlChange, ControlRange, ControlValue,
    Fader, FaderHorizontal, Knob, LevelMeter, MeterValue, SetControlValue, SetToggleValue,
    ToggleButton,
};

/// The contract this module implements. Draft: the verb set and shapes are
/// the first embodiment of the ARexx-style app-control contract and feed the
/// eventual spec; consumers should expect `v0` to move.
pub const APP_CONTROL_CONTRACT: &str = "ctk-app-control.v0";

/// Why an Bus caller failed the current same-node provenance gate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LocalCallerError {
    UnregisteredCaller,
    RemoteIdentityUnavailable,
}

/// Accept only deliveries stamped local by the recipient's noded.
///
/// `broker_origin` is broker-owned: noded strips every client spelling and
/// overwrites it from connection state on delivery. Absence fails closed, so
/// CTK mutation requires the matching broker release.
pub(crate) fn authorize_local_caller(request: &InboundRequest) -> Result<(), LocalCallerError> {
    let asserted = request.headers.keys().any(|name| {
        name.eq_ignore_ascii_case("source_peer")
            || name.eq_ignore_ascii_case("permissions")
            || name.eq_ignore_ascii_case("signed_ident")
    });
    if asserted {
        return Err(LocalCallerError::RemoteIdentityUnavailable);
    }
    let mut origins = request
        .headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("broker_origin"))
        .map(|(_, value)| value.as_str());
    if origins.next() != Some("local") || origins.next().is_some() {
        return Err(LocalCallerError::RemoteIdentityUnavailable);
    }
    if !is_bus_service_name(&request.from) {
        return Err(LocalCallerError::UnregisteredCaller);
    }
    Ok(())
}

fn is_bus_service_name(value: &str) -> bool {
    let bytes = value.as_bytes();
    (2..=31).contains(&bytes.len())
        && bytes[0].is_ascii_lowercase()
        && bytes[1..]
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
}

// `ControlMeta` is attached by feature-independent spawners (the mixer board
// builds without `bus`), so the component itself lives in `widgets`; this
// module remains its consumer and canonical re-export for app-control users.
pub use crate::widgets::ControlMeta;

/// The engine tag every CTK app reports: the concrete renderer arm, per the
/// 2026-07-17 view/engine naming decision (`skia`/`bevy`/`egui`/`html`… —
/// never a category like "gpu").
pub const APP_ENGINE: &str = "bevy";

/// One registered app-port handler's input. The transport-owned request stays
/// intact for parameter/provenance handling; `app_name` is the bridge's live
/// broker identity and is supplied separately so handlers never need to drain
/// or answer through [`BusBridge`] themselves.
pub struct AppPortRequest {
    pub request: InboundRequest,
    pub app_name: String,
}

/// Every named verb returns exactly one Bus rc + JSON body to the base port.
pub type AppPortReply = (u8, String);

type AppVerbSystem = SystemId<In<AppPortRequest>, AppPortReply>;

/// Exact command → typed Bevy handler. Private so registration can enforce a
/// single owner per verb and the router remains the sole response owner.
#[derive(Resource, Default)]
struct AppVerbRegistry {
    by_command: HashMap<String, AppVerbSystem>,
}

#[cfg(feature = "actions")]
pub(crate) fn app_verb_registered(app: &App, command: &str) -> bool {
    app.world()
        .get_resource::<AppVerbRegistry>()
        .is_some_and(|registry| registry.by_command.contains_key(command))
}

/// Register an exact app-local verb handler with the base port.
///
/// The handler is an ordinary Bevy system and may declare any valid
/// [`SystemParam`](bevy::ecs::system::SystemParam). Duplicate verbs, reserved
/// `app.describe`, and commands outside the app-port vocabulary fail at app
/// construction rather than becoming order-dependent at runtime. The reusable
/// action plugin owns the exact `action.invoke` and `actions.*` exceptions.
pub trait AppPortAppExt {
    fn register_app_verb<M>(
        &mut self,
        command: impl Into<String>,
        handler: impl IntoSystem<In<AppPortRequest>, AppPortReply, M> + 'static,
    ) -> &mut Self;
}

impl AppPortAppExt for App {
    fn register_app_verb<M>(
        &mut self,
        command: impl Into<String>,
        handler: impl IntoSystem<In<AppPortRequest>, AppPortReply, M> + 'static,
    ) -> &mut Self {
        let command = command.into();
        assert!(
            self.is_plugin_added::<AppPortPlugin>(),
            "register AppPortPlugin before app-port verbs"
        );
        assert!(
            is_app_port_command(&command),
            "invalid app-port verb: {command}"
        );
        assert_ne!(
            command, "app.describe",
            "app.describe is owned by AppPortPlugin"
        );
        self.init_resource::<AppVerbRegistry>();
        assert!(
            !self
                .world()
                .resource::<AppVerbRegistry>()
                .by_command
                .contains_key(&command),
            "duplicate app-port verb registration: {command}"
        );
        let system = self.world_mut().register_system(handler);
        self.world_mut()
            .resource_mut::<AppVerbRegistry>()
            .by_command
            .insert(command, system);
        self
    }
}

/// App identity reported by `app.describe`.
#[derive(Resource, Clone, Debug)]
pub struct AppControlInfo {
    pub title: String,
    /// The view this app serves (`mixer`, `wave`, `pianoroll`, …) — the
    /// role a caller discovers it by, orthogonal to the engine drawing it.
    pub view: String,
}

/// `BusWidget::id → Entity` for every registered control.
#[derive(Resource, Default)]
pub struct ControlRegistry {
    by_id: HashMap<String, Entity>,
}

impl ControlRegistry {
    pub fn get(&self, id: &str) -> Option<Entity> {
        self.by_id.get(id).copied()
    }

    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }
}

/// The one app-port drain/dispatch/reply stage.
#[derive(bevy::ecs::schedule::SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub struct AppPortSystems;

/// Base Bus app port: `app.describe` plus exact registered named verbs.
///
/// An [`BusBridge`] must already be installed by [`BusBridgePlugin`] or a
/// transport plugin which owns one.
pub struct AppPortPlugin {
    pub title: String,
    pub view: String,
}

impl AppPortPlugin {
    pub fn new(title: impl Into<String>, view: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            view: view.into(),
        }
    }
}

impl Plugin for AppPortPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(AppControlInfo {
            title: self.title.clone(),
            view: self.view.clone(),
        })
        .init_resource::<AppVerbRegistry>();
        let describe = app.world_mut().register_system(describe_app);
        let replaced = app
            .world_mut()
            .resource_mut::<AppVerbRegistry>()
            .by_command
            .insert("app.describe".into(), describe);
        assert!(replaced.is_none(), "app.describe registered twice");
        app.add_systems(Update, route_app_port.in_set(AppPortSystems));
    }
}

/// Optional widget-derived `app.controls.{list,get,set}` surface.
pub struct WidgetControlPlugin;

impl Plugin for WidgetControlPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ControlRegistry>()
            // Unregistration runs FIRST so a despawn + same-id respawn in one
            // frame vacates the id before the new entity registers (the reverse
            // order would reject the replacement against the stale entry, then
            // delete the id entirely). Registration precedes routing so a
            // control spawned this frame is already addressable when its first
            // request drains.
            .add_systems(
                Update,
                (unregister_controls, register_controls)
                    .chain()
                    .before(AppPortSystems),
            )
            .register_app_verb("app.controls.list", handle_widget_request)
            .register_app_verb("app.controls.get", handle_widget_request)
            .register_app_verb("app.controls.set", handle_widget_request);
    }
}

/// Compatibility composition: the base port plus widget-control verbs.
pub struct AppControlPlugin {
    pub title: String,
    pub view: String,
}

impl AppControlPlugin {
    pub fn new(title: impl Into<String>, view: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            view: view.into(),
        }
    }
}

impl Plugin for AppControlPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins((
            AppPortPlugin::new(self.title.clone(), self.view.clone()),
            WidgetControlPlugin,
        ));
    }
}

fn register_controls(
    added: Query<(Entity, &BusWidget), Added<BusWidget>>,
    mut registry: ResMut<ControlRegistry>,
) {
    for (entity, widget) in &added {
        // Layout spacers (neither queryable nor writable) stay unlisted.
        if !widget.queryable && !widget.writable {
            continue;
        }
        match registry.by_id.entry(widget.id.clone()) {
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(entity);
            }
            std::collections::hash_map::Entry::Occupied(existing) if *existing.get() == entity => {}
            std::collections::hash_map::Entry::Occupied(_) => {
                // Unique ids are a contract invariant; a duplicate keeps the
                // first registrant so an id never silently re-targets.
                warn!(
                    id = widget.id.as_str(),
                    "duplicate BusWidget id rejected from the app-control registry"
                );
            }
        }
    }
}

fn unregister_controls(
    mut removed: RemovedComponents<BusWidget>,
    mut registry: ResMut<ControlRegistry>,
    widgets: Query<(Entity, &BusWidget)>,
) {
    let mut vacated = false;
    for entity in removed.read() {
        let before = registry.by_id.len();
        registry.by_id.retain(|_, registered| *registered != entity);
        vacated |= registry.by_id.len() != before;
    }
    if !vacated {
        return;
    }
    // A removal can vacate an id whose earlier duplicate registrant was
    // rejected but still lives; promote any such survivor so the id keeps
    // answering (removals are rare — a full sweep is fine here).
    for (entity, widget) in &widgets {
        if (widget.queryable || widget.writable) && !registry.by_id.contains_key(&widget.id) {
            registry.by_id.insert(widget.id.clone(), entity);
        }
    }
}

/// Everything `list`/`get`/`set` read from one control entity. The marker
/// flags derive the contract `kind`; [`ControlRange`] supplies live
/// `min`/`max`/`step` (the scrubber's range moves with the song length).
type ControlQueryData = (
    &'static BusWidget,
    Option<&'static ControlValue>,
    Option<&'static ControlRange>,
    Option<&'static MeterValue>,
    Option<&'static ControlMeta>,
    Has<Checked>,
    Has<InteractionDisabled>,
    // Both drag signals: SliderDragState.dragging flips on pointer-down,
    // before the first value event inserts ActiveControlGesture.
    (Has<ActiveControlGesture>, Option<&'static SliderDragState>),
    (
        Has<Fader>,
        Has<FaderHorizontal>,
        Has<Knob>,
        Has<ToggleButton>,
        Has<ActionButton>,
        Has<LevelMeter>,
    ),
);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ControlClass {
    Number,
    Bool,
    Action,
    Meter,
}

impl ControlClass {
    fn value_type(self) -> &'static str {
        match self {
            Self::Number => "number",
            Self::Bool => "bool",
            Self::Action => "action",
            Self::Meter => "meter",
        }
    }
}

/// Derive the contract `kind` + value class from the widget marker components.
fn classify(
    (fader, hfader, knob, toggle, action, meter): (bool, bool, bool, bool, bool, bool),
) -> Option<(&'static str, ControlClass)> {
    // Action wins over the underlying button widget; hfader over fader.
    if action {
        return Some(("button", ControlClass::Action));
    }
    if toggle {
        return Some(("toggle", ControlClass::Bool));
    }
    if hfader {
        return Some(("hfader", ControlClass::Number));
    }
    if fader {
        return Some(("fader", ControlClass::Number));
    }
    if knob {
        return Some(("knob", ControlClass::Number));
    }
    if meter {
        return Some(("meter", ControlClass::Meter));
    }
    None
}

/// Drain once, dispatch once, respond once. Requests are collected before any
/// handler runs so registered systems may freely access the world (including
/// the bridge for outbound calls) without aliasing the inbound receiver borrow.
pub(crate) fn route_app_port(world: &mut World) {
    let (app_name, requests) = {
        let bridge = world.resource::<BusBridge>();
        (
            bridge.service_name().to_string(),
            bridge.drain_inbound().collect::<Vec<_>>(),
        )
    };

    for request in requests {
        let (rc, body) = dispatch_app_request(world, &app_name, request.clone());
        if let Err(error) = world
            .resource::<BusBridge>()
            .try_respond(&request, rc, body)
        {
            warn!(
                command = request.command.as_str(),
                error = error.as_str(),
                "app-control response could not be queued"
            );
        }
    }
}

/// Verbs the dispatch-level gate skips: read-only discovery keeps its open
/// contract, and `action.invoke`/`app.controls.set` gate themselves in their
/// handlers with verb-specific error messages. Every OTHER verb — named app
/// verbs included — can mutate app state (quit, transport, theme,
/// constrained-root loads), so it fails closed here before handler lookup.
fn dispatch_gate_skips(command: &str) -> bool {
    matches!(
        command,
        "app.describe"
            | "app.controls.list"
            | "app.controls.get"
            | "actions.list"
            | "actions.describe"
            | "app.controls.set"
            | "action.invoke"
    )
}

pub(crate) fn dispatch_app_request(
    world: &mut World,
    app_name: &str,
    request: InboundRequest,
) -> AppPortReply {
    let command = request.command.clone();
    if !dispatch_gate_skips(&command) {
        if let Err(error) = authorize_local_caller(&request) {
            return error_reply(match error {
                LocalCallerError::UnregisteredCaller => {
                    "app verbs require a registered same-node caller"
                }
                LocalCallerError::RemoteIdentityUnavailable => {
                    "remote ingress is closed until authenticated provenance is available"
                }
            });
        }
    }
    let handler = world
        .resource::<AppVerbRegistry>()
        .by_command
        .get(&command)
        .copied();
    let Some(handler) = handler else {
        return error_reply("unknown app verb");
    };
    match world.run_system_with(
        handler,
        AppPortRequest {
            request,
            app_name: app_name.to_string(),
        },
    ) {
        Ok(reply) => reply,
        Err(error) => {
            warn!(command = command.as_str(), %error, "app-control handler failed");
            error_reply("app verb handler failed")
        }
    }
}

fn is_app_port_command(command: &str) -> bool {
    command.starts_with("app.")
        || matches!(
            command,
            "action.invoke" | "actions.list" | "actions.describe"
        )
}

fn describe_app(
    In(input): In<AppPortRequest>,
    info: Res<AppControlInfo>,
    registry: Option<Res<ControlRegistry>>,
    verbs: Res<AppVerbRegistry>,
) -> AppPortReply {
    // The exact app.* verbs this port answers, sorted for a stable reply — an
    // agent discovers what it can DO here, not just that the app exists.
    let mut verb_names: Vec<&str> = verbs.by_command.keys().map(String::as_str).collect();
    verb_names.sort_unstable();
    (
        0,
        json!({
            "contract": APP_CONTROL_CONTRACT,
            "app": input.app_name,
            "title": info.title,
            // Role discovery: ask "who serves view=mixer" instead of
            // parsing service names. The name is <view>-<engine>-<pid>;
            // these fields are the authoritative decomposition.
            "view": info.view,
            "engine": APP_ENGINE,
            "controls": registry.as_deref().map_or(0, ControlRegistry::len),
            "verbs": verb_names,
        })
        .to_string(),
    )
}

fn handle_widget_request(
    In(input): In<AppPortRequest>,
    registry: Res<ControlRegistry>,
    controls: Query<ControlQueryData>,
    mut commands: Commands,
) -> AppPortReply {
    let request = &input.request;
    match request.command.as_str() {
        "app.controls.list" => (0, list_controls(&registry, &controls)),
        "app.controls.get" => {
            let params = RequestParams::new(request);
            let Some((entity, id)) = lookup(&params, &registry) else {
                return error_reply("unknown control id");
            };
            get_control(entity, &id, &controls)
        }
        "app.controls.set" => {
            if authorize_local_caller(request).is_err() {
                return error_reply("control mutation requires a registered same-node caller");
            }
            let params = RequestParams::new(request);
            let Some((entity, id)) = lookup(&params, &registry) else {
                return error_reply("unknown control id");
            };
            set_control(&params, entity, &id, &controls, &mut commands)
        }
        // Registration maps only the three exact widget verbs here. Keep the
        // guard defensive in case a SystemId is ever aliased incorrectly.
        _ => error_reply("unknown control verb"),
    }
}

/// Request arguments, read header-first with body-JSON fallback (see the
/// module doc: Mix's `send` k=v pairs arrive as a JSON body object).
struct RequestParams<'r> {
    request: &'r InboundRequest,
    body: Option<serde_json::Map<String, serde_json::Value>>,
}

impl<'r> RequestParams<'r> {
    fn new(request: &'r InboundRequest) -> Self {
        let body = serde_json::from_str::<serde_json::Value>(&request.body)
            .ok()
            .and_then(|value| match value {
                serde_json::Value::Object(fields) => Some(fields),
                _ => None,
            });
        Self { request, body }
    }

    fn get(&self, key: &str) -> Option<serde_json::Value> {
        if let Some(value) = self.request.headers.get(key) {
            return Some(serde_json::Value::String(value.clone()));
        }
        self.body.as_ref()?.get(key).cloned()
    }

    fn get_str(&self, key: &str) -> Option<String> {
        match self.get(key)? {
            serde_json::Value::String(value) => Some(value),
            other => Some(other.to_string()),
        }
    }
}

fn error_reply(message: &str) -> (u8, String) {
    (10, json!({ "error": message }).to_string())
}

fn lookup(params: &RequestParams<'_>, registry: &ControlRegistry) -> Option<(Entity, String)> {
    let id = params.get_str("target")?;
    registry.get(&id).map(|entity| (entity, id))
}

/// Contract note: `list` reports metadata only — `get` owns live values.
fn list_controls(registry: &ControlRegistry, controls: &Query<ControlQueryData>) -> String {
    let mut ids: Vec<_> = registry.by_id.keys().collect();
    ids.sort();
    let entries: Vec<_> = ids
        .into_iter()
        .filter_map(|id| {
            let (widget, _, range, _, meta, _, _, _, markers) =
                controls.get(registry.get(id)?).ok()?;
            let (kind, class) = classify(markers)?;
            let kind = meta.and_then(|meta| meta.kind.as_deref()).unwrap_or(kind);
            let mut entry = json!({
                "id": widget.id,
                "kind": kind,
                "value_type": class.value_type(),
                // Action buttons are valueless: triggerable, never readable.
                "queryable": widget.queryable && class != ControlClass::Action,
                "writable": widget.writable,
            });
            let fields = entry.as_object_mut().expect("entry is an object");
            if class == ControlClass::Number {
                if let Some(range) = range {
                    fields.insert("min".into(), json!(range.min));
                    fields.insert("max".into(), json!(range.max));
                    fields.insert("step".into(), json!(range.step));
                }
            }
            if let Some(meta) = meta {
                if let Some(unit) = &meta.unit {
                    fields.insert("unit".into(), json!(unit));
                }
                if !meta.choices.is_empty() {
                    fields.insert("choices".into(), json!(meta.choices));
                }
                if let Some(action) = &meta.action {
                    fields.insert("action".into(), json!(action));
                }
            }
            Some(entry)
        })
        .collect();
    json!({ "controls": entries }).to_string()
}

fn get_control(entity: Entity, id: &str, controls: &Query<ControlQueryData>) -> (u8, String) {
    let Ok((widget, value, _, meter, _, checked, _, _, markers)) = controls.get(entity) else {
        return error_reply("unknown control id");
    };
    let Some((_, class)) = classify(markers) else {
        return error_reply("control has no recognised widget kind");
    };
    if !widget.queryable {
        return error_reply("control is not queryable");
    }
    let value = match class {
        ControlClass::Number => match value {
            Some(value) => json!(value.0),
            None => return error_reply("control has no value"),
        },
        ControlClass::Bool => json!(checked),
        ControlClass::Action => return error_reply("control is not queryable"),
        ControlClass::Meter => match meter {
            Some(meter) => json!(meter.lanes[..usize::from(meter.lane_count).min(2)]
                .iter()
                .map(|lane| {
                    json!({
                        "level": lane.level,
                        "peak": lane.peak,
                        "hold": lane.hold,
                        "clipped": lane.clipped,
                    })
                })
                .collect::<Vec<_>>()),
            None => return error_reply("control has no value"),
        },
    };
    (0, json!({ "id": id, "value": value }).to_string())
}

/// Drive a control as the user would: canonicalise, update the view through
/// the non-emitting programmatic events, then emit the final [`ControlChange`]
/// (or [`Activate`] for a button) so the app's ordinary write pipeline runs.
fn set_control(
    params: &RequestParams<'_>,
    entity: Entity,
    id: &str,
    controls: &Query<ControlQueryData>,
    commands: &mut Commands,
) -> (u8, String) {
    let Ok((widget, _, range, _, _, _, disabled, (gesture_active, drag_state), markers)) =
        controls.get(entity)
    else {
        return error_reply("unknown control id");
    };
    let gesture_active = gesture_active || drag_state.is_some_and(|drag_state| drag_state.dragging);
    let Some((_, class)) = classify(markers) else {
        return error_reply("control has no recognised widget kind");
    };
    if !widget.writable {
        return error_reply("control is not writable");
    }
    // Drive-as-the-user-would includes the refusals: a disabled control
    // rejects the write outright (for every class — a disabled button must
    // not report RC 0 while dispatching nothing).
    if disabled {
        return error_reply("control is disabled");
    }
    // A control mid-pointer-drag is the user's: a remote final commit here
    // would clear the gesture state under the drag and let its cancel path
    // discard the remote value. Busy, try again.
    if gesture_active {
        return (
            11,
            json!({ "error": "control is being manipulated" }).to_string(),
        );
    }
    let raw = params.get("value");
    let applied = match class {
        ControlClass::Number => {
            let Some(parsed) = raw.as_ref().and_then(parse_number) else {
                return error_reply("value must be a number");
            };
            if !parsed.is_finite() {
                return error_reply("value must be finite");
            }
            let value = match range {
                Some(range) => range.canonicalise(parsed),
                None => parsed,
            };
            commands.trigger(SetControlValue {
                source: entity,
                value,
            });
            commands.trigger(ControlChange {
                source: entity,
                value,
                is_final: true,
            });
            json!(value)
        }
        ControlClass::Bool => {
            let Some(value) = raw.as_ref().and_then(parse_bool) else {
                return error_reply("value must be a bool");
            };
            commands.trigger(SetToggleValue {
                source: entity,
                value,
            });
            commands.trigger(ControlChange {
                source: entity,
                value: if value { 1.0 } else { 0.0 },
                is_final: true,
            });
            json!(value)
        }
        ControlClass::Action => {
            // Valueless: `set` IS the press. InteractionDisabled is respected
            // by the widget's own Activate observer.
            commands.trigger(Activate { entity });
            json!(null)
        }
        ControlClass::Meter => return error_reply("control is not writable"),
    };
    // "Applied" here = validated and locally dispatched; any downstream
    // daemon write is asynchronous and reports through the app's own state.
    (0, json!({ "id": id, "applied": applied }).to_string())
}

/// A number, or its string form (a header value is always a string).
fn parse_number(raw: &serde_json::Value) -> Option<f32> {
    match raw {
        serde_json::Value::Number(number) => number.as_f64().map(|value| value as f32),
        serde_json::Value::String(text) => text.parse::<f32>().ok(),
        _ => None,
    }
}

/// A bool, or the usual textual forms (a header value is always a string).
fn parse_bool(raw: &serde_json::Value) -> Option<bool> {
    match raw {
        serde_json::Value::Bool(value) => Some(*value),
        serde_json::Value::String(text) => match text.as_str() {
            "true" | "1" | "on" => Some(true),
            "false" | "0" | "off" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use bevy::ecs::observer::On;
    use bevy::prelude::ResMut;
    use bevy::ui::InteractionDisabled;

    use super::*;
    use crate::widgets::{
        action_button, fader_sized, knob_sized, level_meter_sized, toggle_button_sized,
        CtkWidgetsPlugin, NumericControlProps, ValueMapping,
    };

    #[derive(Resource, Default)]
    struct Changes(Vec<(Entity, f32, bool)>);

    fn record(change: On<ControlChange>, mut changes: ResMut<Changes>) {
        changes
            .0
            .push((change.source, change.value, change.is_final));
    }

    fn test_app() -> App {
        let mut app = App::new();
        let (bridge, _peer) = crate::bus::test_bridge("test-app");
        app.insert_resource(bridge)
            .add_plugins((CtkWidgetsPlugin, AppControlPlugin::new("Test App", "mixer")))
            .init_resource::<Changes>()
            .add_observer(record);
        app
    }

    fn knob_props(id: &str) -> NumericControlProps {
        NumericControlProps::new(
            id,
            0.0,
            ControlRange {
                min: -18.0,
                max: 18.0,
                step: 0.1,
                detent: None,
            },
            ValueMapping::linear(-18.0, 18.0).unwrap(),
        )
    }

    fn request(command: &str, pairs: &[(&str, &str)]) -> InboundRequest {
        let mut headers = pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect::<BTreeMap<_, _>>();
        headers
            .entry("broker_origin".into())
            .or_insert_with(|| "local".into());
        InboundRequest {
            connection_generation: 1,
            from: "tester".into(),
            command: command.into(),
            headers,
            body: String::new(),
            reply_id: Some("42".into()),
        }
    }

    fn call(app: &mut App, request: &InboundRequest) -> (u8, serde_json::Value) {
        let (rc, body) = dispatch_app_request(app.world_mut(), "test-app", request.clone());
        app.update();
        (rc, serde_json::from_str(&body).expect("reply body is JSON"))
    }

    #[derive(Resource, Default)]
    struct TestVerbCalls(usize);

    fn test_verb(In(input): In<AppPortRequest>, mut calls: ResMut<TestVerbCalls>) -> AppPortReply {
        calls.0 += 1;
        (
            0,
            json!({ "command": input.request.command, "calls": calls.0 }).to_string(),
        )
    }

    fn gated_test_app() -> App {
        let mut app = App::new();
        let (bridge, _peer) = crate::bus::test_bridge("studio-bevy-42");
        app.insert_resource(bridge)
            .init_resource::<TestVerbCalls>()
            .add_plugins(AppPortPlugin::new("Studio", "studio"))
            .register_app_verb("app.test", test_verb);
        app
    }

    #[test]
    fn named_verbs_reject_unauthorised_callers() {
        let mut app = gated_test_app();

        let mut anonymous = request("app.test", &[]);
        anonymous.from.clear();
        let (rc, body) = call(&mut app, &anonymous);
        assert_eq!(rc, 10);
        assert!(body["error"]
            .as_str()
            .unwrap()
            .contains("registered same-node caller"));

        let asserted = request("app.test", &[("signed_ident", "spoof")]);
        let (rc, body) = call(&mut app, &asserted);
        assert_eq!(rc, 10);
        assert!(body["error"].as_str().unwrap().contains("remote ingress"));

        let mut unstamped = request("app.test", &[]);
        unstamped.headers.remove("broker_origin");
        let (rc, _) = call(&mut app, &unstamped);
        assert_eq!(rc, 10);

        assert_eq!(
            app.world().resource::<TestVerbCalls>().0,
            0,
            "no gated request may reach the handler"
        );

        let (rc, _) = call(&mut app, &request("app.test", &[]));
        assert_eq!(rc, 0, "canonical local caller still dispatches");
    }

    #[test]
    fn discovery_stays_reachable_without_the_gate() {
        let mut app = gated_test_app();
        let mut anonymous = request("app.describe", &[]);
        anonymous.from.clear();
        anonymous.headers.remove("broker_origin");
        let (rc, body) = call(&mut app, &anonymous);
        assert_eq!(rc, 0, "read-only discovery keeps its open contract");
        assert_eq!(body["contract"], APP_CONTROL_CONTRACT);
    }

    #[test]
    fn base_port_routes_registered_verbs_and_owns_one_reply() {
        let mut app = App::new();
        let (bridge, peer) = crate::bus::test_bridge("studio-bevy-42");
        app.insert_resource(bridge)
            .init_resource::<TestVerbCalls>()
            .add_plugins(AppPortPlugin::new("Studio", "studio"))
            .register_app_verb("app.test", test_verb);

        let mut describe = request("app.describe", &[]);
        describe.reply_id = Some("describe".into());
        peer.send(describe);
        let mut registered = request("app.test", &[]);
        registered.reply_id = Some("registered".into());
        peer.send(registered);
        let mut unknown = request("app.nope", &[]);
        unknown.reply_id = Some("unknown".into());
        peer.send(unknown);

        app.update();

        let responses = peer.drain_responses();
        assert_eq!(responses.len(), 3, "one response per inbound request");
        let response = |command: &str| {
            responses
                .iter()
                .find(|response| response.command == command)
                .unwrap_or_else(|| panic!("response for {command}"))
        };
        let describe = response("app.describe");
        assert_eq!(describe.rc, 0);
        let describe: serde_json::Value = serde_json::from_str(&describe.body).unwrap();
        assert_eq!(describe["app"], "studio-bevy-42");
        assert_eq!(describe["title"], "Studio");
        assert_eq!(describe["view"], "studio");
        assert_eq!(describe["controls"], 0);

        let registered = response("app.test");
        assert_eq!(registered.rc, 0);
        assert_eq!(app.world().resource::<TestVerbCalls>().0, 1);

        let unknown = response("app.nope");
        assert_eq!(unknown.rc, 10);
        let unknown: serde_json::Value = serde_json::from_str(&unknown.body).unwrap();
        assert_eq!(unknown["error"], "unknown app verb");
    }

    #[test]
    fn fire_and_forget_registered_verb_executes_without_a_response() {
        let mut app = App::new();
        let (bridge, peer) = crate::bus::test_bridge("studio");
        app.insert_resource(bridge)
            .init_resource::<TestVerbCalls>()
            .add_plugins(AppPortPlugin::new("Studio", "mixer"))
            .register_app_verb("app.transport.play", test_verb);

        let mut command = request("app.transport.play", &[]);
        command.reply_id = None;
        peer.send(command);
        app.update();

        assert_eq!(app.world().resource::<TestVerbCalls>().0, 1);
        assert!(peer.drain_responses().is_empty());
    }

    #[test]
    fn registry_keeps_first_registrant_and_skips_spacers() {
        let mut app = test_app();
        let first = app
            .world_mut()
            .spawn(knob_sized(knob_props("trim"), 26.0))
            .id();
        app.update();
        // A duplicate id keeps the first entity.
        let second = app
            .world_mut()
            .spawn(knob_sized(knob_props("trim"), 26.0))
            .id();
        // A spacer (neither queryable nor writable) never registers.
        app.world_mut().spawn(BusWidget {
            id: "spacer".into(),
            queryable: false,
            writable: false,
        });
        app.update();

        let registry = app.world().resource::<ControlRegistry>();
        assert_eq!(registry.len(), 1);
        assert_eq!(registry.get("trim"), Some(first));

        // Removing the registrant promotes the surviving duplicate — the id
        // keeps answering rather than dying with the first entity.
        app.world_mut().entity_mut(first).despawn();
        app.update();
        let registry = app.world().resource::<ControlRegistry>();
        assert_eq!(registry.get("trim"), Some(second));

        app.world_mut().entity_mut(second).despawn();
        app.update();
        assert!(app.world().resource::<ControlRegistry>().is_empty());
    }

    #[test]
    fn describe_and_list_report_metadata_not_values() {
        let mut app = test_app();
        app.world_mut().spawn((
            fader_sized(knob_props("fader"), 12.0, 300.0),
            ControlMeta::unit("dB"),
        ));
        app.world_mut()
            .spawn(toggle_button_sized("mute", 22.0, 18.0));
        app.world_mut().spawn((
            action_button("play", 56.0, 26.0),
            ControlMeta::action("transport.state=playing"),
        ));
        app.world_mut().spawn(level_meter_sized(
            "meter",
            MeterValue::default(),
            12.0,
            300.0,
        ));
        app.update();

        let (rc, describe) = call(&mut app, &request("app.describe", &[]));
        assert_eq!(rc, 0);
        assert_eq!(describe["contract"], APP_CONTROL_CONTRACT);
        assert_eq!(describe["app"], "test-app");
        assert_eq!(describe["title"], "Test App");
        assert_eq!(describe["view"], "mixer");
        assert_eq!(describe["engine"], "bevy");
        assert_eq!(describe["controls"], 4);
        // The port advertises the verbs it answers: describe + the widget verbs.
        let verbs: Vec<&str> = describe["verbs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(verbs.contains(&"app.describe"));
        assert!(verbs.contains(&"app.controls.set"));

        let (rc, list) = call(&mut app, &request("app.controls.list", &[]));
        assert_eq!(rc, 0);
        let controls = list["controls"].as_array().unwrap();
        assert_eq!(controls.len(), 4);
        let by_id = |id: &str| {
            controls
                .iter()
                .find(|entry| entry["id"] == id)
                .unwrap_or_else(|| panic!("{id} listed"))
        };

        let fader = by_id("fader");
        assert_eq!(fader["kind"], "fader");
        assert_eq!(fader["value_type"], "number");
        assert_eq!(fader["unit"], "dB");
        assert_eq!(fader["min"], -18.0);
        assert_eq!(fader["max"], 18.0);
        assert!(fader["writable"].as_bool().unwrap());
        // Metadata only: values belong to `get`.
        assert!(fader.get("value").is_none());

        assert_eq!(by_id("mute")["value_type"], "bool");

        let play = by_id("play");
        assert_eq!(play["kind"], "button");
        assert_eq!(play["action"], "transport.state=playing");
        // Valueless: triggerable but never readable.
        assert!(!play["queryable"].as_bool().unwrap());

        let meter = by_id("meter");
        assert_eq!(meter["value_type"], "meter");
        assert!(!meter["writable"].as_bool().unwrap());
    }

    #[test]
    fn get_returns_domain_values_and_rejects_actions() {
        let mut app = test_app();
        app.world_mut().spawn(knob_sized(knob_props("trim"), 26.0));
        app.world_mut()
            .spawn(toggle_button_sized("mute", 22.0, 18.0));
        app.world_mut().spawn(action_button("play", 56.0, 26.0));
        app.update();

        let (rc, body) = call(
            &mut app,
            &request("app.controls.get", &[("target", "trim")]),
        );
        assert_eq!(rc, 0);
        assert_eq!(body["value"], 0.0);

        let (rc, body) = call(
            &mut app,
            &request("app.controls.get", &[("target", "mute")]),
        );
        assert_eq!(rc, 0);
        assert_eq!(body["value"], false);

        let (rc, _) = call(
            &mut app,
            &request("app.controls.get", &[("target", "play")]),
        );
        assert_eq!(rc, 10);

        let (rc, _) = call(
            &mut app,
            &request("app.controls.get", &[("target", "nope")]),
        );
        assert_eq!(rc, 10);
    }

    #[test]
    fn set_canonicalises_and_drives_the_user_pipeline() {
        let mut app = test_app();
        let knob = app
            .world_mut()
            .spawn(knob_sized(knob_props("trim"), 26.0))
            .id();
        let mute = app
            .world_mut()
            .spawn(toggle_button_sized("mute", 22.0, 18.0))
            .id();
        let play = app
            .world_mut()
            .spawn(action_button("play", 56.0, 26.0))
            .id();
        app.update();

        // Out-of-range numbers clamp to the control's range (99 → 18).
        let (rc, body) = call(
            &mut app,
            &request("app.controls.set", &[("target", "trim"), ("value", "99")]),
        );
        assert_eq!(rc, 0);
        assert_eq!(body["applied"], 18.0);
        assert_eq!(app.world().get::<ControlValue>(knob).unwrap().0, 18.0);

        let (rc, body) = call(
            &mut app,
            &request("app.controls.set", &[("target", "mute"), ("value", "on")]),
        );
        assert_eq!(rc, 0);
        assert_eq!(body["applied"], true);
        assert!(app.world().entity(mute).contains::<Checked>());

        let (rc, _) = call(
            &mut app,
            &request("app.controls.set", &[("target", "play")]),
        );
        assert_eq!(rc, 0);

        // Each set flowed through the ordinary final-ControlChange pipeline.
        let changes = &app.world().resource::<Changes>().0;
        assert_eq!(
            changes.as_slice(),
            [(knob, 18.0, true), (mute, 1.0, true), (play, 0.0, true)]
        );
    }

    #[test]
    fn controls_set_rejects_remote_provenance_and_allows_local_callers() {
        let mut app = test_app();
        let knob = app
            .world_mut()
            .spawn(knob_sized(knob_props("trim"), 26.0))
            .id();
        app.update();

        let local = request("app.controls.set", &[("target", "trim"), ("value", "-6")]);
        let (rc, _) = call(&mut app, &local);
        assert_eq!(rc, 0);
        assert_eq!(app.world().get::<ControlValue>(knob).unwrap().0, -6.0);

        let mut remote = request("app.controls.set", &[("target", "trim"), ("value", "4")]);
        remote
            .headers
            .insert("source_peer".into(), "remote.example".into());
        let (rc, body) = call(&mut app, &remote);
        assert_eq!(rc, 10);
        assert_eq!(
            body["error"],
            "control mutation requires a registered same-node caller"
        );
        assert_eq!(app.world().get::<ControlValue>(knob).unwrap().0, -6.0);

        let mut stripped = request("app.controls.set", &[("target", "trim"), ("value", "2")]);
        stripped.from.clear();
        let (rc, _) = call(&mut app, &stripped);
        assert_eq!(rc, 10);
        assert_eq!(app.world().get::<ControlValue>(knob).unwrap().0, -6.0);

        let mut unstamped = request("app.controls.set", &[("target", "trim"), ("value", "2")]);
        unstamped.headers.remove("broker_origin");
        let (rc, _) = call(&mut app, &unstamped);
        assert_eq!(rc, 10);
        let mut mesh = request("app.controls.set", &[("target", "trim"), ("value", "2")]);
        mesh.headers.insert("broker_origin".into(), "mesh".into());
        let (rc, _) = call(&mut app, &mesh);
        assert_eq!(rc, 10);
        assert_eq!(app.world().get::<ControlValue>(knob).unwrap().0, -6.0);
    }

    #[test]
    fn body_json_params_serve_mix_callers_and_headers_win() {
        let mut app = test_app();
        let knob = app
            .world_mut()
            .spawn(knob_sized(knob_props("trim"), 26.0))
            .id();
        app.update();

        // Mix `send app app.controls.set target="trim" value=-6` arrives as a
        // JSON body object with typed values, not Bus headers.
        let mut mix_style = request("app.controls.set", &[]);
        mix_style.body = r#"{"target":"trim","value":-6}"#.into();
        let (rc, body) = call(&mut app, &mix_style);
        assert_eq!(rc, 0);
        assert_eq!(body["applied"], -6.0);
        assert_eq!(app.world().get::<ControlValue>(knob).unwrap().0, -6.0);

        // An explicit header beats a conflicting body field.
        let mut conflicted = request("app.controls.get", &[("target", "trim")]);
        conflicted.body = r#"{"target":"bogus"}"#.into();
        let (rc, body) = call(&mut app, &conflicted);
        assert_eq!(rc, 0);
        assert_eq!(body["value"], -6.0);
    }

    #[test]
    fn set_rejects_bad_values_disabled_paths_and_unknown_verbs() {
        let mut app = test_app();
        app.world_mut().spawn(knob_sized(knob_props("trim"), 26.0));
        app.world_mut().spawn(level_meter_sized(
            "meter",
            MeterValue::default(),
            12.0,
            300.0,
        ));
        // Disabled controls refuse remote writes outright, every class.
        let stop = app
            .world_mut()
            .spawn((action_button("stop", 56.0, 26.0), InteractionDisabled))
            .id();
        app.world_mut()
            .spawn((toggle_button_sized("solo", 22.0, 18.0), InteractionDisabled));
        app.update();

        let (rc, _) = call(
            &mut app,
            &request("app.controls.set", &[("target", "trim"), ("value", "loud")]),
        );
        assert_eq!(rc, 10);
        let (rc, _) = call(
            &mut app,
            &request("app.controls.set", &[("target", "trim"), ("value", "NaN")]),
        );
        assert_eq!(rc, 10);
        let (rc, _) = call(
            &mut app,
            &request("app.controls.set", &[("target", "meter"), ("value", "1")]),
        );
        assert_eq!(rc, 10);
        let (rc, _) = call(&mut app, &request("app.controls.frobnicate", &[]));
        assert_eq!(rc, 10);

        let (rc, _) = call(
            &mut app,
            &request("app.controls.set", &[("target", "stop")]),
        );
        assert_eq!(rc, 10);
        let (rc, _) = call(
            &mut app,
            &request("app.controls.set", &[("target", "solo"), ("value", "on")]),
        );
        assert_eq!(rc, 10);
        let _ = stop;

        // A control mid-drag belongs to the user: remote set answers busy.
        let dragged = app
            .world_mut()
            .spawn((knob_sized(knob_props("pan"), 26.0), ActiveControlGesture))
            .id();
        app.update();
        let (rc, body) = call(
            &mut app,
            &request("app.controls.set", &[("target", "pan"), ("value", "1")]),
        );
        assert_eq!(rc, 11);
        assert_eq!(body["error"], "control is being manipulated");
        let _ = dragged;

        assert!(app.world().resource::<Changes>().0.is_empty());
    }

    #[test]
    fn same_frame_despawn_and_respawn_keeps_the_id_answering() {
        let mut app = test_app();
        let first = app
            .world_mut()
            .spawn(knob_sized(knob_props("trim"), 26.0))
            .id();
        app.update();

        // Despawn + same-id respawn inside one frame: unregistration runs
        // first, so the replacement claims the vacated id.
        app.world_mut().entity_mut(first).despawn();
        let second = app
            .world_mut()
            .spawn(knob_sized(knob_props("trim"), 26.0))
            .id();
        app.update();

        let registry = app.world().resource::<ControlRegistry>();
        assert_eq!(registry.len(), 1);
        assert_eq!(registry.get("trim"), Some(second));
    }

    #[test]
    fn control_meta_kind_overrides_the_marker_kind() {
        let mut app = test_app();
        app.world_mut().spawn((
            fader_sized(knob_props("position"), 300.0, 24.0),
            ControlMeta {
                kind: Some("scrubber".into()),
                ..ControlMeta::unit("s")
            },
        ));
        app.update();

        let (rc, list) = call(&mut app, &request("app.controls.list", &[]));
        assert_eq!(rc, 0);
        assert_eq!(list["controls"][0]["kind"], "scrubber");
        assert_eq!(list["controls"][0]["unit"], "s");
    }
}
