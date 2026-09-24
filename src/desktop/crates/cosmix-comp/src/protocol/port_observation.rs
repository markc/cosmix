//! Calloop-owned semantic observation reduction and the bounded Bus outbox.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use chrono::{SecondsFormat, TimeZone, Utc};
use cosmix_bus::bus::BusMessage;
use crossbeam_channel::{Receiver, Sender, TryRecvError, TrySendError};
use serde::Serialize;
use serde_json::{Value, json};
use smithay::output::Output;
use smithay::reexports::calloop::{
    LoopHandle, RegistrationToken,
    timer::{TimeoutAction, Timer},
};
use smithay::reexports::wayland_server::{Resource, backend::ClientId};
use tokio::sync::Notify;

use crate::hotspot_scene::{HotspotBridge, Square, View as HotspotView};
use crate::port::{ControlReply, PortControl, PortSetRequest};

use super::{
    CursorPositionSnapshot, StackBand, SurfaceId, WaylandState,
    corner::{Corner, CornerConfig, CornerDetector, CornerEvent},
    pointer_observation::{LEASE, PointerLease, PointerPosition, PointerSample},
    port_snapshot::{
        BindingRowSnapshot, CompSnapshot, EdgeCounts, FocusSnapshot, LayerSnapshot, OutputSnapshot,
        SurfaceSnapshot, WindowSnapshot, WorkspaceRowSnapshot, project_focus, project_output,
        project_outputs, project_stack, project_surface_by_id, project_window_row, snapshot,
        volatile_path,
    },
    window_control::WindowTargetError,
    workspaces::{WORKSPACE_COUNT_MAX, WorkspaceRefusal, WorkspaceTarget},
};

pub(crate) const PROPS_TOPIC_SUFFIX: &str = "props.changed";
pub(crate) const SURFACE_MAPPED_TOPIC_SUFFIX: &str = "surface.mapped";
pub(crate) const SURFACE_UNMAPPED_TOPIC_SUFFIX: &str = "surface.unmapped";
pub(crate) const FOCUS_TOPIC_SUFFIX: &str = "focus.changed";
pub(crate) const OUTPUT_TOPIC_SUFFIX: &str = "output.changed";
pub(crate) const CORNER_ENTERED_TOPIC_SUFFIX: &str = "corner.entered";
pub(crate) const CORNER_LEFT_TOPIC_SUFFIX: &str = "corner.left";
pub(crate) const CORNER_CLICKED_TOPIC_SUFFIX: &str = "corner.clicked";
pub(crate) const CORNER_CLICKED_V2_TOPIC_SUFFIX: &str = "corner.clicked.v2";
const DISCOVERY_PATH: &str = "input.corners.discovery";
pub(crate) const POINTER_TOPIC_SUFFIX: &str = "pointer.changed";
pub(crate) const PANEL_COMMAND_TOPIC_SUFFIX: &str = "panel.command";
/// The `input.corners.holders` leaf. Quoin switches to command-driven
/// reveal/conceal on this leaf, so it may only be true in a comp build that
/// also enforces the conceal on a stalled Quoin: holder tracking, the conceal
/// timer, enforcement, disconnect cleanup and resynchronisation all exist.
pub(crate) const HOLDER_PLANE_AVAILABLE: bool = true;
/// Shell design §4.3: a pointer holder releases only after the pointer has
/// been away from the hotspot, the panel and its popups this long. Focus and
/// popup releases are deliberate and conceal at once.
pub(crate) const CONCEAL_DELAY: Duration = Duration::from_millis(800);
/// Shell design §7: how long Quoin has to apply a conceal (its slide is
/// 200 ms) before comp checks whether it is alive. Enforcement is the
/// stalled-client path, never the animation.
pub(crate) const ENFORCE_GRACE: Duration = Duration::from_millis(1000);
/// How long a liveness probe (an unchanged configure re-sent to the owner's
/// layers) waits for its `ack_configure`. A stopped client cannot answer; a
/// busy but live one does.
pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_millis(1000);

const PANEL_ARGS: &[&str] =
    &["output", "edge", "surface", "holder", "acquire", "mode", "generation"];

#[derive(Clone, Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PanelRequest {
    pub(crate) output: String,
    pub(crate) edge: String,
    pub(crate) surface: String,
    pub(crate) holder: Option<String>,
    pub(crate) acquire: Option<bool>,
    pub(crate) mode: Option<String>,
    /// `comp.panel.mode` only: the reporter's Bus connection generation. A
    /// report from another generation is a new Bus incarnation of the
    /// holder, whose predecessor's explicit holds must not survive.
    pub(crate) generation: Option<u64>,
    /// The broker-stamped sender (the registered service, or empty for an
    /// anonymous caller). Set by the dispatch boundary, never by the body.
    #[serde(skip)]
    pub(crate) sender: String,
}

impl PanelRequest {
    /// Refusals name the offending argument, like the window/input verbs.
    pub(crate) fn parse(verb: &str, args: &Value) -> Result<Self, ControlReply> {
        let invalid = |field: &str| ControlReply::InvalidArgs {
            field: field.to_owned(),
            allowed: PANEL_ARGS,
        };
        let Some(object) = args.as_object() else {
            return Err(invalid("args"));
        };
        if let Some(unknown) = object.keys().find(|name| !PANEL_ARGS.contains(&name.as_str())) {
            return Err(invalid(unknown.as_str()));
        }
        // A missing required field or a wrong JSON type names that field too.
        type Check = (&'static str, bool, fn(&Value) -> bool);
        let typed: [Check; 7] = [
            ("output", true, Value::is_string),
            ("edge", true, Value::is_string),
            ("surface", true, Value::is_string),
            ("holder", false, Value::is_string),
            ("acquire", false, Value::is_boolean),
            ("mode", false, Value::is_string),
            ("generation", false, Value::is_u64),
        ];
        for (name, required, ok) in typed {
            let valid = object.get(name).filter(|value| !value.is_null()).map_or(!required, ok);
            if !valid {
                return Err(invalid(name));
            }
        }
        let request: Self = serde_json::from_value(args.clone()).map_err(|_| invalid("args"))?;
        let hold = verb == "comp.panel.hold";
        let bounded = |value: &str| !value.is_empty() && value.len() <= 256;
        let checks = [
            ("output", bounded(&request.output)),
            ("surface", bounded(&request.surface)),
            ("edge", matches!(request.edge.as_str(), "top" | "bottom" | "left" | "right")),
            ("holder", if hold {
                matches!(request.holder.as_deref(), Some("pointer" | "focus" | "popup"))
            } else {
                request.holder.is_none()
            }),
            ("acquire", request.acquire.is_some() == hold),
            ("mode", if hold {
                request.mode.is_none()
            } else {
                matches!(request.mode.as_deref(), Some("hidden" | "pinned" | "docked"))
            }),
            ("generation", !hold || request.generation.is_none()),
        ];
        match checks.into_iter().find(|(_, ok)| !ok) {
            Some((field, _)) => Err(invalid(field)),
            None => Ok(request),
        }
    }
}

/// The holder sets of one `(output, edge)` panel (shell design §4.3): the
/// explicit holds Quoin requests plus the pointer and focus membership comp
/// tracks itself. Pure state with an injected clock; the timer and the
/// Wayland lookups live in [`service_panel_holders`].
#[derive(Debug)]
pub(crate) struct PanelHolders {
    pub(crate) surface: String,
    /// Re-resolved from `surface` on layer lifecycle edges.
    pub(crate) id: Option<SurfaceId>,
    pub(crate) mode: String,
    /// Holder kind -> (token, layer id at acquire). The id is not refreshed:
    /// a replaced layer always carries a new token, so a held id only ever
    /// names the layer it was acquired against.
    pub(crate) held: BTreeMap<String, (String, SurfaceId)>,
    /// The pointer holder comp tracks from its hotspot and hit-testing.
    pub(crate) pointer: PointerHold,
    /// Keyboard focus is on the panel's layer or a held popup's.
    pub(crate) focused: bool,
    /// The last command sent for this edge; `None` while persistent (pinned
    /// and docked panels have no holders) and before the first verdict.
    pub(crate) verdict: Option<bool>,
    /// The Wayland client whose layers this edge belongs to: the Quoin
    /// incarnation. Comp's own identity for the owner — the namespace token
    /// is not one, since any client can copy it. Set by the first layer comp
    /// resolves for the edge; that client's disconnect drops everything it
    /// held ([`PanelHolders::drop_incarnation`]).
    pub(crate) owner: Option<ClientId>,
    /// Popup layers acquired for this edge under `owner`, while they live.
    pub(crate) popups: BTreeSet<SurfaceId>,
    /// The next step of an owed conceal: the grace Quoin has to apply it,
    /// after which comp probes it for liveness.
    pub(crate) enforce_at: Option<Instant>,
    /// Shell design §7, recorded at a conceal that ended a reveal comp
    /// commanded: the owner's layers that were showing then. If any of them
    /// unmaps or goes, Quoin has applied the conceal and nothing is owed; at
    /// the deadline only these, still mapped, are candidates.
    pub(crate) pending: BTreeSet<SurfaceId>,
    /// A conceal just ended a commanded reveal: record `pending` at the
    /// next tracking pass, which has the Wayland facts.
    pub(crate) arm_pending: bool,
    /// An unanswered liveness probe of this edge's layers.
    pub(crate) probe: Option<Probe>,
    /// The owner answered a probe for the current showing state: nothing is
    /// probed again until the next trigger (a commanded conceal, the verdict
    /// revealing, or nothing showing any more).
    pub(crate) quiet: bool,
    /// The owner answered a probe: no press-triggered probe of it before
    /// this ([`PROBE_TIMEOUT`] after the answer).
    pub(crate) probe_rest_until: Option<Instant>,
    /// A probe went unanswered: the owner is stopped. Its popup and focus
    /// holds are dropped, its keyboard focus counts for nothing and its
    /// Exclusive layers lose their grab, until it is heard from again.
    pub(crate) stalled: bool,
    /// The owner's layers comp itself hides and excludes from input: the
    /// candidates still mapped when a probe went unanswered. Never
    /// re-resolved from a token and never grown afterwards; each leaves once
    /// its client unmaps or destroys it.
    pub(crate) enforced: BTreeSet<SurfaceId>,
    /// The registered Bus service whose mode report named this edge's token
    /// (the holder service). Only such a report may give an unowned edge its
    /// owner, and that service leaving the Bus drops its holds.
    pub(crate) reporter: Option<String>,
    /// The token the reporter itself named: the only one an unowned edge
    /// is adopted for. A report from anyone else never replaces it.
    pub(crate) reported_surface: Option<String>,
    /// The reporter's Bus connection generation, when it states one.
    pub(crate) generation: Option<u64>,
}

/// One liveness probe: an unchanged configure re-sent to each candidate
/// layer, answered by any acknowledgement at or after its serial.
#[derive(Clone, Debug)]
pub(crate) struct Probe {
    pub(crate) deadline: Instant,
    pub(crate) serials: Vec<(SurfaceId, smithay::utils::Serial)>,
}

/// The pointer holder. `Lingering` is a released pointer still inside its
/// conceal delay: it holds until the delay ends, and re-entering the hotspot
/// or the panel within it makes the pointer `Inside` again.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum PointerHold {
    #[default]
    Out,
    Inside,
    Lingering(Instant),
}

/// Focus restoration for one held popup (surface ids). Restoration happens
/// only when the popup's own destruction moved focus and focus is still
/// where that destruction put it. Focus leaving the popup while it lives is a
/// deliberate move and cancels the restoration — unless it went to another
/// held popup (a nested menu), which restores back to this one when it closes.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct PopupRestore {
    /// The focus the popup displaced when it took focus; `None` until then.
    pub(crate) prior: Option<u64>,
    /// Where focus went when the popup's destruction moved it.
    pub(crate) fallback: Option<Option<u64>>,
    /// Focus left the popup while it lived, to this surface. A deliberate
    /// move unless that surface becomes (or is) a held popup.
    pub(crate) departed_to: Option<Option<u64>>,
}

/// What comp observed of one panel at a stable dispatch boundary.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Membership {
    /// The pointer dwelled in this edge's hotspot (the corner is engaged).
    pub(crate) dwelled: bool,
    /// The pointer is in this edge's hotspot, dwelled or not.
    pub(crate) hotspot: bool,
    /// The pointer is over the panel's layer or a held popup's.
    pub(crate) surface: bool,
    /// Keyboard focus is on the panel's layer or a held popup's.
    pub(crate) focused: bool,
}

impl PanelHolders {
    fn new(surface: String, id: Option<SurfaceId>) -> Self {
        Self {
            surface,
            id,
            mode: "hidden".into(),
            held: BTreeMap::new(),
            pointer: PointerHold::Out,
            focused: false,
            verdict: None,
            owner: None,
            popups: BTreeSet::new(),
            enforce_at: None,
            pending: BTreeSet::new(),
            arm_pending: false,
            probe: None,
            quiet: false,
            probe_rest_until: None,
            stalled: false,
            enforced: BTreeSet::new(),
            reporter: None,
            reported_surface: None,
            generation: None,
        }
    }

    fn hidden(&self) -> bool {
        self.mode == "hidden"
    }

    /// Follow a verdict just sent (`previous` is the one before it). A
    /// reveal lifts everything owed or enforced. A conceal that ends a reveal
    /// comp commanded owes Quoin's conceal: the showing layers are recorded
    /// on the next pass ([`PanelHolders::pending`]).
    pub(crate) fn note_verdict(&mut self, reveal: bool, previous: Option<bool>) {
        if reveal {
            self.clear_enforcement();
            self.quiet = false;
        } else if previous == Some(true) {
            self.clear_enforcement();
            self.quiet = false;
            self.arm_pending = true;
        }
    }

    /// Something is owed or enforced on this edge.
    pub(crate) fn owed(&self) -> bool {
        !self.enforced.is_empty() || self.enforce_at.is_some() || self.probe.is_some()
    }

    /// Quoin applied the conceal (a recorded layer unmapped or went), or
    /// answered a probe: nothing is owed.
    pub(crate) fn settle_owed(&mut self) {
        self.pending.clear();
        self.enforce_at = None;
        self.probe = None;
    }

    fn clear_enforcement(&mut self) {
        self.settle_owed();
        self.arm_pending = false;
        self.enforced.clear();
    }

    /// The popup and focus holds of a stopped owner end: they are the ones
    /// that can keep a stopped menu or launcher on screen with the keyboard.
    pub(crate) fn stall(&mut self) {
        self.stalled = true;
        self.held.remove("popup");
        self.held.remove("focus");
    }

    /// The holder service's explicit holds end (it left the Bus, or a new Bus
    /// generation of it reported). What comp observes itself — pointer,
    /// focus, the owner and any enforcement — belongs to the Wayland client
    /// and stays.
    pub(crate) fn drop_holds(&mut self) {
        self.held.clear();
    }

    /// The owning Wayland client is gone: every explicit hold, popup and
    /// enforcement of that incarnation goes with it, so none survives into
    /// the next one. The automatic pointer and focus holders are comp's own
    /// observations and follow the normal rules.
    pub(crate) fn drop_incarnation(&mut self) {
        self.owner = None;
        self.drop_holds();
        self.popups.clear();
        self.stalled = false;
        self.quiet = false;
        self.clear_enforcement();
    }

    /// Fold one observation into the automatic holders. The pointer acquires
    /// by dwelling in the hotspot or by entering the panel (whose layer exists
    /// only while it is visible) or a popup it holds; any contact with those
    /// keeps it; leaving all of them starts its conceal delay.
    pub(crate) fn observe(&mut self, seen: Membership, now: Instant) {
        let acquire = seen.dwelled || seen.surface;
        let contact = acquire || seen.hotspot;
        self.pointer = match self.pointer {
            PointerHold::Out if acquire => PointerHold::Inside,
            PointerHold::Out => PointerHold::Out,
            PointerHold::Inside | PointerHold::Lingering(_) if contact => PointerHold::Inside,
            PointerHold::Inside => PointerHold::Lingering(now),
            lingering @ PointerHold::Lingering(_) => lingering,
        };
        self.focused = seen.focused;
    }

    /// A lingering pointer whose delay has run out has released.
    pub(crate) fn expire(&mut self, now: Instant) {
        if let PointerHold::Lingering(since) = self.pointer
            && now >= since + CONCEAL_DELAY
        {
            self.pointer = PointerHold::Out;
        }
    }

    fn holding(&self) -> bool {
        self.pointer != PointerHold::Out || self.focused || !self.held.is_empty()
    }

    /// The one-shot conceal deadline. It exists only while the lingering
    /// pointer is the last holder of a hidden panel, so it is armed by the
    /// last release and cancelled by any holder returning.
    pub(crate) fn conceal_deadline(&self) -> Option<Instant> {
        match self.pointer {
            PointerHold::Lingering(since)
                if self.hidden() && !self.focused && self.held.is_empty() =>
            {
                Some(since + CONCEAL_DELAY)
            }
            _ => None,
        }
    }

    /// The command owed to Quoin: `Some(true)` reveal, `Some(false)` conceal.
    /// Emitted on a change of verdict, or always when `restate` (a hidden
    /// mode report, which may come from a Quoin that has just gone
    /// command-driven and knows nothing of comp's holders).
    pub(crate) fn settle(&mut self, restate: bool) -> Option<bool> {
        if !self.hidden() {
            self.verdict = None;
            return None;
        }
        let holding = self.holding();
        (restate || self.verdict != Some(holding)).then(|| {
            self.verdict = Some(holding);
            holding
        })
    }
}

/// Exact namespace resolution. `candidates` are every layer whose namespace
/// is the token, flagged by whether it is on the requested output. All of
/// them are considered, so the verdict never depends on surface order.
pub(crate) fn resolve_panel_surface(
    candidates: impl IntoIterator<Item = (SurfaceId, bool)>,
) -> Result<Option<SurfaceId>, &'static str> {
    let mut candidates = candidates.into_iter();
    let Some((id, on_output)) = candidates.next() else {
        return Ok(None);
    };
    if candidates.next().is_some() {
        Err("ambiguous_panel_surface")
    } else if on_output {
        Ok(Some(id))
    } else {
        Err("panel_output_mismatch")
    }
}

fn panel_surface_candidates<'a>(
    state: &'a WaylandState,
    token: &'a str,
    output: &'a str,
) -> impl Iterator<Item = (SurfaceId, bool)> + 'a {
    state.surfaces.values().filter_map(move |record| {
        let super::SurfaceRole::Layer(layer) = &record.role else {
            return None;
        };
        (layer.surface.namespace() == token).then(|| {
            (record.id, layer.output.output().is_some_and(|bound| bound.name() == output))
        })
    })
}

/// Idempotent explicit requests, returning the command owed to Quoin (see
/// [`PanelHolders::settle`]): a hidden mode report always re-states the
/// verdict, a hold only reports a change of it. Automatic membership and the
/// conceal deadline attach to the same state at the dispatch boundary.
fn apply_panel_request(
    panels: &mut BTreeMap<(String, String), PanelHolders>,
    request: &PanelRequest,
    id: Option<SurfaceId>,
) -> Option<bool> {
    let key = (request.output.clone(), request.edge.clone());
    // A release for an edge comp has no state for has nothing to release;
    // it must not invent an entry keyed to a token comp never saw.
    if request.acquire == Some(false) && !panels.contains_key(&key) {
        return None;
    }
    let panel = panels.entry(key).or_insert_with(|| PanelHolders::new(request.surface.clone(), id));
    if let Some(mode) = &request.mode {
        panel.surface.clone_from(&request.surface);
        panel.id = id;
        panel.mode.clone_from(mode);
        // A report comes from a live Quoin: comp's exclusion lifts. A
        // persistent panel is meant to show, so nothing is owed; a hidden one
        // that still owed a conceal owes it again with a fresh grace (the
        // caller re-arms it), so a Quoin that stalls again stays bounded.
        if mode != "hidden" {
            panel.drop_holds();
        }
        panel.clear_enforcement();
        return panel.settle(true);
    }
    if panel.mode != "hidden" { return None; }
    if let (Some(holder), Some(acquire)) = (&request.holder, request.acquire) {
        if acquire {
            if let Some(id) = id { panel.held.insert(holder.clone(), (request.surface.clone(), id)); }
        } else if panel.held.get(holder).is_some_and(|(surface, _)| surface == &request.surface) {
            panel.held.remove(holder);
        }
    }
    panel.settle(false)
}

pub(crate) fn topic_name(service: &str, suffix: &str) -> String {
    format!("{service}.{suffix}")
}

type PendingPropChanges = BTreeMap<String, (PropValue, PropValue, &'static str)>;

const OUTBOX_CAPACITY: usize = 256;

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(untagged)]
pub(crate) enum PropValue {
    Null(()),
    Bool(bool),
    U64(u64),
    I32(i32),
    U32(u32),
    F32(f32),
    F64(f64),
    String(String),
    U64List(Vec<u64>),
    BindingRows(Vec<BindingRowSnapshot>),
    WorkspaceRows(Vec<WorkspaceRowSnapshot>),
    OutputRow(Box<OutputSnapshot>),
    SurfaceRow(Box<SurfaceSnapshot>),
    WindowRow(Box<WindowSnapshot>),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum SetValidationError {
    UnknownPath,
    ReadOnly,
    InvalidValue {
        path: String,
        expected: &'static str,
        range: &'static str,
    },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum ValidatedCornerValue {
    Enabled(bool),
    DeadzonePx(f64),
    DwellMs(u64),
    VelocityMaxPxS(f64),
    Affordance(bool),
    Discovery(bool),
}

impl PropValue {
    fn null() -> Self {
        Self::Null(())
    }

    pub(crate) fn wire_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ObservationRecord {
    PanelCommand {
        output: String,
        edge: String,
        surface: String,
        reveal: bool,
        event_seq: u64,
    },
    PointerChanged {
        sample: PointerSample,
        event_seq: u64,
    },
    PropsChanged {
        path: String,
        old: PropValue,
        new: PropValue,
        unix_ms: i64,
        cause: &'static str,
        event_seq: u64,
    },
    SurfaceMapped {
        id: u64,
        role: String,
        foreign_id: Option<String>,
        window: SurfaceEdgeWindow,
        event_seq: u64,
    },
    SurfaceUnmapped {
        id: u64,
        role: String,
        foreign_id: Option<String>,
        window: SurfaceEdgeWindow,
        event_seq: u64,
    },
    FocusChanged {
        keyboard: Option<u64>,
        previous: Option<u64>,
        exclusive_latch: Option<u64>,
        event_seq: u64,
    },
    OutputChanged {
        output: String,
        row: OutputSnapshot,
        event_seq: u64,
    },
    CornerEntered {
        output: String,
        corner: Corner,
        dwell_ms: u64,
        event_seq: u64,
    },
    CornerLeft {
        output: String,
        corner: Corner,
        dwell_ms: u64,
        event_seq: u64,
    },
    CornerClicked {
        output: String,
        corner: Corner,
        dwell_ms: u64,
        event_seq: u64,
    },
    CornerClickedV2 {
        output: String,
        corner: Corner,
        dwell_ms: u64,
        button: &'static str,
        kind: &'static str,
        modifiers: Vec<&'static str>,
        event_seq: u64,
    },
}

impl ObservationRecord {
    pub(crate) fn event_seq(&self) -> u64 {
        match self {
            Self::PanelCommand { event_seq, .. } => *event_seq,
            Self::PointerChanged { event_seq, .. } => *event_seq,
            Self::PropsChanged { event_seq, .. }
            | Self::SurfaceMapped { event_seq, .. }
            | Self::SurfaceUnmapped { event_seq, .. }
            | Self::FocusChanged { event_seq, .. }
            | Self::OutputChanged { event_seq, .. }
            | Self::CornerEntered { event_seq, .. }
            | Self::CornerClicked { event_seq, .. }
            | Self::CornerClickedV2 { event_seq, .. }
            | Self::CornerLeft { event_seq, .. } => *event_seq,
        }
    }

    pub(crate) fn topic_suffix(&self) -> &'static str {
        match self {
            Self::PanelCommand { .. } => PANEL_COMMAND_TOPIC_SUFFIX,
            Self::PointerChanged { .. } => POINTER_TOPIC_SUFFIX,
            Self::PropsChanged { .. } => PROPS_TOPIC_SUFFIX,
            Self::SurfaceMapped { .. } => SURFACE_MAPPED_TOPIC_SUFFIX,
            Self::SurfaceUnmapped { .. } => SURFACE_UNMAPPED_TOPIC_SUFFIX,
            Self::FocusChanged { .. } => FOCUS_TOPIC_SUFFIX,
            Self::OutputChanged { .. } => OUTPUT_TOPIC_SUFFIX,
            Self::CornerEntered { .. } => CORNER_ENTERED_TOPIC_SUFFIX,
            Self::CornerLeft { .. } => CORNER_LEFT_TOPIC_SUFFIX,
            Self::CornerClicked { .. } => CORNER_CLICKED_TOPIC_SUFFIX,
            Self::CornerClickedV2 { .. } => CORNER_CLICKED_V2_TOPIC_SUFFIX,
        }
    }

    pub(crate) fn wire(&self) -> BusMessage {
        let mut message = BusMessage::new();
        message.set("command", self.topic_suffix());
        message.set("event_seq", &self.event_seq().to_string());
        message.body = match self {
            Self::PanelCommand { output, edge, surface, reveal, event_seq } => json!({
                "version": 1, "output": output, "edge": edge, "surface": surface,
                "action": if *reveal { "reveal" } else { "conceal" }, "event_seq": event_seq,
            }).to_string(),
            Self::CornerClickedV2 {
                output,
                corner,
                dwell_ms,
                button,
                kind,
                modifiers,
                event_seq,
            } => json!({
                "output": output,
                "corner": corner.name(),
                "dwell_ms": dwell_ms,
                "button": button,
                "kind": kind,
                "modifiers": modifiers,
                "event_seq": event_seq,
            })
            .to_string(),
            Self::PointerChanged { sample, event_seq } => {
                let mut value = serde_json::to_value(sample).expect("finite pointer sample");
                value["event_seq"] = json!(event_seq);
                value.to_string()
            }
            Self::PropsChanged {
                path,
                old,
                new,
                unix_ms,
                cause,
                event_seq,
            } => {
                message.set("path", path);
                message.set("cause", cause);
                json!({
                    "path": path,
                    "old": old.wire_value(),
                    "new": new.wire_value(),
                    "ts": rfc3339_millis(*unix_ms),
                    "cause": cause,
                    "event_seq": event_seq,
                })
                .to_string()
            }
            Self::SurfaceMapped {
                id,
                role,
                foreign_id,
                window,
                event_seq,
            }
            | Self::SurfaceUnmapped {
                id,
                role,
                foreign_id,
                window,
                event_seq,
            } => {
                let mut body = json!({
                    "id": id,
                    "role": role,
                    "generation": window.generation,
                    "app_id": window.app_id.as_deref(),
                    "title": window.title.as_deref(),
                    "event_seq": event_seq,
                });
                if let Some(foreign_id) = foreign_id {
                    body.as_object_mut()
                        .expect("surface event body is an object")
                        .insert("foreign_id".into(), json!(foreign_id));
                }
                body.to_string()
            }
            Self::FocusChanged {
                keyboard,
                previous,
                exclusive_latch,
                event_seq,
            } => json!({
                "keyboard": keyboard,
                "previous": previous,
                "exclusive_latch": exclusive_latch,
                "event_seq": event_seq,
            })
            .to_string(),
            Self::OutputChanged {
                output,
                row,
                event_seq,
            } => json!({
                "output": output,
                "geometry": {
                    "x": row.x,
                    "y": row.y,
                    "width": row.width,
                    "height": row.height,
                },
                "usable": row.usable,
                "event_seq": event_seq,
            })
            .to_string(),
            Self::CornerEntered {
                output,
                corner,
                dwell_ms,
                event_seq,
            }
            | Self::CornerLeft {
                output,
                corner,
                dwell_ms,
                event_seq,
            }
            | Self::CornerClicked {
                output,
                corner,
                dwell_ms,
                event_seq,
            } => json!({
                "output": output,
                "corner": corner.name(),
                "dwell_ms": dwell_ms,
                "event_seq": event_seq,
            })
            .to_string(),
        };
        message
    }
}

const TOPIC_SUFFIXES: [&str; 11] = [
    PROPS_TOPIC_SUFFIX,
    SURFACE_MAPPED_TOPIC_SUFFIX,
    SURFACE_UNMAPPED_TOPIC_SUFFIX,
    FOCUS_TOPIC_SUFFIX,
    OUTPUT_TOPIC_SUFFIX,
    CORNER_ENTERED_TOPIC_SUFFIX,
    CORNER_LEFT_TOPIC_SUFFIX,
    CORNER_CLICKED_TOPIC_SUFFIX,
    POINTER_TOPIC_SUFFIX,
    CORNER_CLICKED_V2_TOPIC_SUFFIX,
    PANEL_COMMAND_TOPIC_SUFFIX,
];

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct AffectedTopics(u16);

impl AffectedTopics {
    fn index(suffix: &str) -> usize {
        TOPIC_SUFFIXES
            .iter()
            .position(|candidate| *candidate == suffix)
            .expect("every observation has a fixed topic suffix")
    }

    pub(crate) fn insert(&mut self, suffix: &str) {
        let index = Self::index(suffix);
        self.0 |= 1 << index;
    }

    pub(crate) fn merge(&mut self, other: Self) {
        self.0 |= other.0;
    }

    pub(crate) fn remove(&mut self, suffix: &str) {
        self.0 &= !(1 << Self::index(suffix));
    }

    pub(crate) fn contains(self, suffix: &str) -> bool {
        self.0 & (1 << Self::index(suffix)) != 0
    }

    pub(crate) fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub(crate) fn iter(self) -> impl Iterator<Item = &'static str> {
        TOPIC_SUFFIXES
            .into_iter()
            .enumerate()
            .filter_map(move |(index, suffix)| (self.0 & (1 << index) != 0).then_some(suffix))
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum LossCause {
    OutboxOverflow,
    PublisherLoss,
}

impl LossCause {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::OutboxOverflow => "outbox.overflow",
            Self::PublisherLoss => "publisher.loss",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct LossInterval {
    pub(crate) first_lost_seq: u64,
    pub(crate) last_lost_seq: u64,
    pub(crate) topics: AffectedTopics,
    pub(crate) cause: LossCause,
}

impl LossInterval {
    pub(crate) fn from_record(record: &ObservationRecord, cause: LossCause) -> Self {
        let mut topics = AffectedTopics::default();
        topics.insert(record.topic_suffix());
        Self {
            first_lost_seq: record.event_seq(),
            last_lost_seq: record.event_seq(),
            topics,
            cause,
        }
    }

    pub(crate) fn merge(&mut self, other: Self) {
        self.first_lost_seq = self.first_lost_seq.min(other.first_lost_seq);
        self.last_lost_seq = self.last_lost_seq.max(other.last_lost_seq);
        self.topics.merge(other.topics);
        self.cause = self.cause.max(other.cause);
    }
}

pub(crate) struct ObservationOutbox {
    pub(crate) records: Receiver<OutboxRecord>,
    pub(crate) capacity: usize,
}

pub(crate) struct OutboxRecord {
    pub(crate) record: ObservationRecord,
    pub(crate) preceding_loss: Option<LossInterval>,
}

fn rfc3339_millis(unix_ms: i64) -> String {
    Utc.timestamp_millis_opt(unix_ms)
        .single()
        .unwrap_or_else(Utc::now)
        .to_rfc3339_opts(SecondsFormat::Millis, true)
}

pub(crate) struct ObservationProducer {
    record_sender: Sender<OutboxRecord>,
    record_eviction: Receiver<OutboxRecord>,
    pending_loss: Option<LossInterval>,
    lost_count: Arc<AtomicU64>,
    notifier: Arc<Notify>,
}

impl ObservationProducer {
    /// Fixed-cost calloop boundary: bounded channels allocate their storage at
    /// construction. One offer performs at most two sends and one eviction; it
    /// must never grow a Vec/VecDeque/Box, serialise JSON, wait, poll, lock or
    /// loop over the queue.
    pub(crate) fn offer(&mut self, record: ObservationRecord) {
        let record = OutboxRecord {
            record,
            preceding_loss: self.pending_loss.take(),
        };
        match self.record_sender.try_send(record) {
            Ok(()) => self.notifier.notify_one(),
            Err(TrySendError::Disconnected(record)) => {
                self.fold_lost_record(record, LossCause::PublisherLoss);
            }
            Err(TrySendError::Full(record)) => {
                self.replace_one_oldest(record);
            }
        }
    }

    fn replace_one_oldest(&mut self, mut record: OutboxRecord) {
        if let Some(loss) = record.preceding_loss.take() {
            self.merge_pending_loss(loss);
        }
        match self.record_eviction.try_recv() {
            Ok(evicted) => {
                self.fold_lost_record(evicted, LossCause::OutboxOverflow);
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.fold_lost_record(record, LossCause::PublisherLoss);
                return;
            }
        }

        record.preceding_loss = self.pending_loss.take();
        match self.record_sender.try_send(record) {
            Ok(()) => self.notifier.notify_one(),
            Err(TrySendError::Disconnected(record)) => {
                self.fold_lost_record(record, LossCause::PublisherLoss);
            }
            Err(TrySendError::Full(record)) => {
                debug_assert!(false, "one consumer or one eviction leaves one outbox slot");
                self.fold_lost_record(record, LossCause::OutboxOverflow);
            }
        }
    }

    fn fold_lost_record(&mut self, record: OutboxRecord, cause: LossCause) {
        if let Some(loss) = record.preceding_loss {
            self.merge_pending_loss(loss);
        }
        self.merge_pending_loss(LossInterval::from_record(&record.record, cause));
        self.lost_count.fetch_add(1, Ordering::AcqRel);
    }

    fn merge_pending_loss(&mut self, loss: LossInterval) {
        if let Some(pending) = self.pending_loss.as_mut() {
            pending.merge(loss);
        } else {
            self.pending_loss = Some(loss);
        }
    }

    pub(crate) fn notifier(&self) -> Arc<Notify> {
        Arc::clone(&self.notifier)
    }
}

pub(crate) fn outbox(lost_count: Arc<AtomicU64>) -> (ObservationProducer, ObservationOutbox) {
    outbox_with_capacity(lost_count, OUTBOX_CAPACITY)
}

fn outbox_with_capacity(
    lost_count: Arc<AtomicU64>,
    capacity: usize,
) -> (ObservationProducer, ObservationOutbox) {
    assert!(capacity > 0, "observation data lane must have capacity");
    let (record_sender, records) = crossbeam_channel::bounded(capacity);
    let notifier = Arc::new(Notify::new());
    (
        ObservationProducer {
            record_sender,
            record_eviction: records.clone(),
            pending_loss: None,
            lost_count,
            notifier,
        },
        ObservationOutbox { records, capacity },
    )
}

#[cfg(test)]
pub(crate) fn test_outbox(
    lost_count: Arc<AtomicU64>,
    capacity: usize,
) -> (ObservationProducer, ObservationOutbox) {
    outbox_with_capacity(lost_count, capacity)
}

#[derive(Clone, Debug)]
struct SurfaceEdgeStart {
    mapped: bool,
    role: String,
    foreign_id: Option<String>,
    window: SurfaceEdgeWindow,
}

/// Identity fields a map edge carries, so a streaming observer can match
/// the window without a props read. Title and app id are null while a
/// session lock is active, as in the read tree.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct SurfaceEdgeWindow {
    pub(crate) generation: u64,
    pub(crate) app_id: Option<std::sync::Arc<str>>,
    pub(crate) title: Option<std::sync::Arc<str>>,
}

#[derive(Clone, Debug)]
struct OutputEdgeStart {
    output: Output,
    row: OutputSnapshot,
}

#[derive(Clone, Copy, Debug)]
struct FocusEdgeStart {
    keyboard: Option<u64>,
    exclusive_latch: Option<u64>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct CornerRegion {
    pub(crate) key_index: usize,
    pub(crate) origin: (f64, f64),
    pub(crate) size: (f64, f64),
}

pub(crate) struct ObservationState {
    pub(crate) panel_holders: BTreeMap<(String, String), PanelHolders>,
    /// The one-shot panel timer and the deadline it is armed for: the
    /// earliest conceal delay or enforcement grace of any edge.
    conceal_timer: Option<RegistrationToken>,
    pub(crate) conceal_deadline: Option<Instant>,
    #[cfg(test)]
    pub(crate) conceal_timer_arms: usize,
    /// Held popup layer (surface id) -> the focus to restore when it closes.
    pub(crate) popup_restores: BTreeMap<u64, PopupRestore>,
    /// The latest keyboard focus change as `(to, from)` surface ids.
    last_focus_change: Option<(Option<u64>, Option<u64>)>,
    /// A holder request ran in this cycle's controls: reconcile after them.
    panel_request_serviced: bool,
    #[cfg(test)]
    pub(crate) conceal_timer_fired: usize,
    /// Every layer some edge enforces ([`PanelHolders::enforced`]): the one
    /// set `recompute_effective_visibility` reads, so comp's hiding and input
    /// exclusion reach these surfaces (and their descendants) and nothing else.
    pub(crate) enforced_surfaces: BTreeSet<SurfaceId>,
    /// Owners that left a liveness probe unanswered
    /// ([`PanelHolders::stalled`]), read by keyboard arbitration.
    pub(crate) stalled_owners: Vec<ClientId>,
    /// The Wayland clients that got a button or key press since the last
    /// tracking pass (the pressed or focused surface's client): a trigger to
    /// probe an owner whose menu or launcher alone keeps its panel shown.
    pub(crate) user_input: Vec<Option<ClientId>>,
    pointer_lease: PointerLease,
    pointer_seen: Option<(CursorPositionSnapshot, bool)>,
    pointer_timer: Option<RegistrationToken>,
    pointer_deadline: Option<Instant>,
    pending_surface_edges: BTreeMap<u64, SurfaceEdgeStart>,
    dirty_surfaces: BTreeMap<u64, &'static str>,
    dirty_outputs: BTreeMap<String, OutputEdgeStart>,
    output_topology_dirty: bool,
    property_dirty_outputs: BTreeMap<String, (Output, &'static str)>,
    pending_focus: Option<FocusEdgeStart>,
    focus_cause: &'static str,
    stack_dirty: Option<&'static str>,
    full_dirty: Option<&'static str>,
    /// A pointer constraint was broken or declined while a corner was
    /// engaged; the next physical motion outside every corner activates
    /// the pending constraint (see `defer_constraint_activation`).
    constraint_activation_deferred: bool,
    watched_baseline: Option<CompSnapshot>,
    pub(crate) corner_config: CornerConfig,
    pub(crate) corner_regions: Vec<CornerRegion>,
    pub(crate) corner_output_keys: Vec<String>,
    corner_detector: CornerDetector,
    // None retains ownership of a cancelled/completed press until release.
    corner_presses: BTreeMap<u32, Option<CornerPress>>,
    corner_output: Option<usize>,
    corner_clock: Instant,
    corner_timer: Option<RegistrationToken>,
    corner_timer_deadline_ms: Option<u64>,
    #[cfg(test)]
    corner_timer_arms: usize,
    /// The renderer's affordance view (shell design §8.1, §8.5, §8.7).
    hotspots: Option<HotspotBridge>,
    /// The last recognised corner release: square index and time.
    hotspot_flash: Option<(usize, Instant)>,
    /// Phase origin of the discovery blink while `corner_config.discovery`.
    discovery_since: Option<Instant>,
    loop_handle: LoopHandle<'static, WaylandState>,
    producer: ObservationProducer,
    event_seq: u64,
    event_seq_exhausted: bool,
    event_seq_watermark: Arc<AtomicU64>,
}

struct CornerPress {
    output: String,
    corner: Corner,
    dwell_ms: u64,
    position: (f64, f64),
    modifiers: Vec<&'static str>,
}

impl ObservationState {
    pub(super) fn new(
        producer: ObservationProducer,
        event_seq_watermark: Arc<AtomicU64>,
        loop_handle: LoopHandle<'static, WaylandState>,
    ) -> Self {
        let corner_config = CornerConfig::default();
        Self {
            panel_holders: BTreeMap::new(),
            conceal_timer: None,
            conceal_deadline: None,
            #[cfg(test)]
            conceal_timer_arms: 0,
            popup_restores: BTreeMap::new(),
            enforced_surfaces: BTreeSet::new(),
            stalled_owners: Vec::new(),
            user_input: Vec::new(),
            last_focus_change: None,
            panel_request_serviced: false,
            #[cfg(test)]
            conceal_timer_fired: 0,
            pointer_lease: PointerLease::default(),
            pointer_seen: None,
            pointer_timer: None,
            pointer_deadline: None,
            pending_surface_edges: BTreeMap::new(),
            dirty_surfaces: BTreeMap::new(),
            dirty_outputs: BTreeMap::new(),
            output_topology_dirty: false,
            property_dirty_outputs: BTreeMap::new(),
            pending_focus: None,
            focus_cause: "wayland.focus",
            stack_dirty: None,
            full_dirty: None,
            constraint_activation_deferred: false,
            watched_baseline: None,
            corner_config,
            corner_regions: Vec::new(),
            corner_output_keys: Vec::new(),
            corner_detector: CornerDetector::new(corner_config, (0.0, 0.0)),
            corner_presses: BTreeMap::new(),
            corner_output: None,
            corner_clock: Instant::now(),
            corner_timer: None,
            corner_timer_deadline_ms: None,
            #[cfg(test)]
            corner_timer_arms: 0,
            hotspots: None,
            hotspot_flash: None,
            discovery_since: None,
            loop_handle,
            producer,
            event_seq: 0,
            event_seq_exhausted: false,
            event_seq_watermark,
        }
    }

    fn next_seq(&mut self) -> Option<u64> {
        if self.event_seq_exhausted {
            return None;
        }
        self.event_seq += 1;
        self.event_seq_exhausted = self.event_seq == u64::MAX;
        self.event_seq_watermark
            .store(self.event_seq, Ordering::Release);
        Some(self.event_seq)
    }

    fn offer(&mut self, build: impl FnOnce(u64) -> ObservationRecord) -> Option<u64> {
        let sequence = self.next_seq()?;
        self.producer.offer(build(sequence));
        Some(sequence)
    }

    pub(crate) fn drop_watch(&mut self) {
        self.watched_baseline = None;
    }
}

impl WaylandState {
    pub(crate) fn mark_surface_before_change(&mut self, id: SurfaceId) {
        if self.observations.pending_surface_edges.contains_key(&id.0) {
            return;
        }
        let Some(object) = self.surface_objects.get(&id) else {
            return;
        };
        let Some(record) = self.surfaces.get(object) else {
            return;
        };
        let start = SurfaceEdgeStart {
            mapped: record.mapped,
            role: record.role.kind().to_string(),
            foreign_id: self.foreign_toplevel_identifiers.get(&id).cloned(),
            window: edge_window(self, record),
        };
        self.observations.pending_surface_edges.insert(id.0, start);
    }

    pub(crate) fn mark_surface_mapped(
        &mut self,
        surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    ) {
        let Some((id, was_mapped, generation)) = self
            .surfaces
            .get(&surface.id())
            .map(|record| (record.id, record.mapped, record.generation))
        else {
            return;
        };
        if !was_mapped {
            self.mark_surface_before_change(id);
            self.note_window_mapping(id, generation);
        }
        self.mark_surface_dirty(id, "wayland.map");
        if !was_mapped {
            self.mark_stack_dirty("wayland.map");
        }
    }

    pub(crate) fn mark_surface_unmapped(
        &mut self,
        surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    ) {
        let Some((id, was_mapped)) = self
            .surfaces
            .get(&surface.id())
            .map(|record| (record.id, record.mapped))
        else {
            return;
        };
        if was_mapped {
            self.mark_surface_before_change(id);
        }
        self.mark_surface_dirty(id, "wayland.unmap");
        if was_mapped {
            self.mark_stack_dirty("wayland.unmap");
        }
    }

    pub(crate) fn mark_surface_dirty(&mut self, id: SurfaceId, cause: &'static str) {
        if self.observations.watched_baseline.is_none() {
            return;
        }
        self.observations
            .dirty_surfaces
            .entry(id.0)
            .or_insert(cause);
    }

    pub(crate) fn mark_stack_dirty(&mut self, cause: &'static str) {
        if self.observations.watched_baseline.is_none() {
            return;
        }
        self.observations.stack_dirty.get_or_insert(cause);
    }

    pub(crate) fn mark_focus_before_change(&mut self, cause: &'static str) {
        if self.observations.pending_focus.is_none() {
            self.observations.pending_focus = Some(project_focus_edge(self));
        }
        if self.observations.watched_baseline.is_none() {
            return;
        }
        self.observations.focus_cause = cause;
    }

    pub(crate) fn mark_output_before_change(&mut self, output: &Output, cause: &'static str) {
        if cause == "output.geometry" {
            self.cancel_corner_presses();
        }
        let Some((key, row)) = project_output(self, output) else {
            if self.observations.watched_baseline.is_some() {
                self.observations.full_dirty.get_or_insert(cause);
            }
            return;
        };
        self.observations
            .dirty_outputs
            .entry(key.clone())
            .or_insert_with(|| OutputEdgeStart {
                output: output.clone(),
                row,
            });
        if self.observations.watched_baseline.is_none() {
            return;
        }
        self.observations
            .property_dirty_outputs
            .entry(key)
            .or_insert_with(|| (output.clone(), cause));
    }

    #[cfg(any(all(feature = "kms-live", not(test)), test))]
    pub(crate) fn mark_all_outputs_before_change(&mut self, cause: &'static str) {
        let Some(projection) = project_outputs(self) else {
            if self.observations.watched_baseline.is_some() {
                self.observations.full_dirty.get_or_insert(cause);
            }
            return;
        };
        for (output, key) in projection.keys {
            let Some(row) = projection.rows.get(&key).cloned() else {
                continue;
            };
            self.observations
                .property_dirty_outputs
                .entry(key.clone())
                .or_insert((output.clone(), cause));
            self.observations
                .dirty_outputs
                .entry(key)
                .or_insert(OutputEdgeStart { output, row });
        }
    }

    #[cfg(any(all(feature = "kms-live", not(test)), test))]
    pub(crate) fn mark_output_topology_before_change(&mut self) {
        self.cancel_corner_presses();
        self.observations.pointer_lease.changed();
        self.mark_all_outputs_before_change("output.geometry");
        self.observations.output_topology_dirty = true;
        self.observations
            .full_dirty
            .get_or_insert("output.geometry");
    }

    /// The pending full-snapshot cause, for tests that assert a path stayed
    /// inert. Read it BEFORE a service cycle: `service_property_diffs`
    /// takes it (and the per-surface marks with it) in the cycle that
    /// runs a verb, so after `dispatch_cycle` it reads `None` whatever the
    /// verb did — a serviced verb's marks are only observable through the
    /// causes of the edges that cycle emitted.
    #[cfg(test)]
    pub(crate) fn full_dirty_cause(&self) -> Option<&'static str> {
        self.observations.full_dirty
    }

    /// A workspace switch, move or count change: `workspaces.*` and every
    /// window row's visibility may have changed, so the next observation
    /// diffs a full snapshot (D7).
    pub(crate) fn mark_workspaces_dirty(&mut self, cause: &'static str) {
        self.observations.full_dirty.get_or_insert(cause);
    }

    /// `xwayland.display` changed (a generation came up or went down): the
    /// leaf lives on the full snapshot, so the next observation diffs one.
    #[cfg(feature = "xwayland")]
    pub(crate) fn mark_xwayland_dirty(&mut self, cause: &'static str) {
        self.observations.full_dirty.get_or_insert(cause);
    }

    pub(crate) fn mark_session_observation_dirty(&mut self) {
        self.mark_focus_before_change("session.lock");
        self.observations.full_dirty.get_or_insert("session.lock");
    }

    pub(crate) fn emit_corner_entered(&mut self, output: String, corner: Corner, dwell_ms: u64) {
        self.observations
            .offer(|event_seq| ObservationRecord::CornerEntered {
                output,
                corner,
                dwell_ms,
                event_seq,
            });
    }

    pub(crate) fn consume_corner_press(&mut self, button: u32) -> bool {
        if self.observations.corner_presses.contains_key(&button) {
            return true;
        }
        if self.session_lock_active() || !self.corner_engaged() {
            return false;
        }
        let action = if let Some(output) = self
            .observations
            .corner_output
            .and_then(|index| self.observations.corner_output_keys.get(index))
            .cloned()
            && let Some(corner) = self.observations.corner_detector.engaged_corner()
            && let Some(dwell_ms) = self.observations.corner_detector.engaged_dwell_ms()
            && (button == super::PRIMARY_POINTER_BUTTON
                || button == super::PRIMARY_POINTER_BUTTON + 1)
        {
            // Read calloop-owned keyboard state now; release may have different modifiers.
            let state = self.keyboard.modifier_state();
            let modifiers = [
                (state.shift, "shift"),
                (state.ctrl, "ctrl"),
                (state.alt, "alt"),
                (state.logo, "super"),
            ]
            .into_iter()
            .filter_map(|(active, name)| active.then_some(name))
            .collect();
            Some(CornerPress {
                output,
                corner,
                dwell_ms,
                position: self.cursor_position,
                modifiers,
            })
        } else {
            None
        };
        self.observations.corner_presses.insert(button, action);
        self.rearm_corner_timer();
        true
    }

    /// Called before modal/lock delivery gates, so ownership survives all resets
    /// and the eventual release cannot reach a client that never saw its press.
    pub(crate) fn consume_corner_release(&mut self, button: u32) -> bool {
        if !self.observations.corner_presses.contains_key(&button) {
            return false;
        }
        if self
            .observations
            .corner_presses
            .get(&button)
            .is_some_and(Option::is_some)
        {
            let position = self.cursor_position;
            let region = self.observations.corner_output.unwrap_or_default();
            self.sample_corner_motion(position, region, (0.0, 0.0));
        }
        if let Some(Some(press)) = self.observations.corner_presses.remove(&button) {
            self.emit_corner_action(press, button);
        }
        self.rearm_corner_timer();
        true
    }

    fn cancel_corner_presses(&mut self) {
        for action in self.observations.corner_presses.values_mut() {
            *action = None;
        }
    }

    fn service_corner_presses(&mut self, position: (f64, f64)) {
        let deadzone = self.observations.corner_config.deadzone_px;
        for action in self.observations.corner_presses.values_mut() {
            let Some(press) = action else { continue };
            if (position.0 - press.position.0).hypot(position.1 - press.position.1) > deadzone {
                *action = None;
            }
        }
    }

    fn emit_corner_action(&mut self, press: CornerPress, button: u32) {
        // Every recognised release is acknowledged with a flash (§8.1).
        if let Some(region) = self
            .observations
            .corner_output_keys
            .iter()
            .position(|key| *key == press.output)
        {
            self.observations.hotspot_flash = Some((
                region * Corner::ALL.len() + press.corner.index(),
                Instant::now(),
            ));
            self.publish_hotspots();
        }
        let left = button == super::PRIMARY_POINTER_BUTTON;
        // The decoder canonicalises unmodified LMB v2 to the legacy sequence.
        // Every such click needs its sibling; every modified click is standalone.
        if left && press.modifiers.is_empty() {
            self.emit_corner_clicked(press.output.clone(), press.corner, press.dwell_ms);
        }
        self.observations
            .offer(|event_seq| ObservationRecord::CornerClickedV2 {
                output: press.output,
                corner: press.corner,
                dwell_ms: press.dwell_ms,
                button: if left { "left" } else { "right" },
                kind: "brief",
                modifiers: press.modifiers,
                event_seq,
            });
    }

    pub(crate) fn emit_corner_clicked(&mut self, output: String, corner: Corner, dwell_ms: u64) {
        self.observations
            .offer(|event_seq| ObservationRecord::CornerClicked {
                output,
                corner,
                dwell_ms,
                event_seq,
            });
    }

    pub(crate) fn emit_corner_left(&mut self, output: String, corner: Corner, dwell_ms: u64) {
        self.observations
            .offer(|event_seq| ObservationRecord::CornerLeft {
                output,
                corner,
                dwell_ms,
                event_seq,
            });
    }

    pub(crate) fn refresh_corner_regions(&mut self) {
        let Some(projection) = project_outputs(self) else {
            self.observations.corner_regions.clear();
            self.reset_corner_detector();
            self.observations.corner_output_keys.clear();
            self.publish_hotspots();
            return;
        };
        let mut keys = Vec::with_capacity(projection.keys.len());
        let mut regions = Vec::with_capacity(projection.keys.len());
        for (_, key) in projection.keys {
            let Some(row) = projection.rows.get(&key) else {
                continue;
            };
            let key_index = keys.len();
            keys.push(key);
            regions.push(CornerRegion {
                key_index,
                origin: (f64::from(row.x), f64::from(row.y)),
                size: (f64::from(row.width), f64::from(row.height)),
            });
        }
        self.observations.corner_output_keys = keys;
        self.observations.corner_regions = regions;
        self.publish_hotspots();
    }

    /// Attach the renderer's affordance bridge and give it the current view.
    pub(crate) fn install_hotspot_bridge(&mut self, bridge: HotspotBridge) {
        self.observations.hotspots = Some(bridge);
        self.publish_hotspots();
    }

    /// Project the corner state into the renderer's affordance view: one
    /// square of `deadzone_px` LOGICAL units at each corner of each output,
    /// indexed `region * 4 + Corner::index`. Called only on state changes,
    /// never per motion sample.
    fn publish_hotspots(&self) {
        let Some(bridge) = &self.observations.hotspots else {
            return;
        };
        let config = self.observations.corner_config;
        let squares = self
            .observations
            .corner_regions
            .iter()
            .flat_map(|region| {
                let (x, y) = region.origin;
                let (width, height) = region.size;
                let side = config.deadzone_px.min(width).min(height).max(0.0);
                Corner::ALL.map(|corner| {
                    let right = matches!(corner, Corner::TopRight | Corner::BottomRight);
                    let bottom = matches!(corner, Corner::BottomLeft | Corner::BottomRight);
                    Square {
                        x: if right { x + width - side } else { x },
                        y: if bottom { y + height - side } else { y },
                        side,
                    }
                })
            })
            .collect();
        let hover = self
            .observations
            .corner_output
            .zip(self.observations.corner_detector.engaged_corner())
            .map(|(region, corner)| region * Corner::ALL.len() + corner.index());
        bridge.set(HotspotView {
            enabled: config.affordance && config.enabled && config.valid(),
            squares,
            hover,
            flash: self.observations.hotspot_flash,
            discovery: self
                .observations
                .discovery_since
                .filter(|_| config.discovery),
        });
    }

    /// The first reveal of any kind ends the discovery flash (§8.5). A
    /// hover or click reaches here; a keyboard reveal is the shell's, and it
    /// ends discovery by writing `input.corners.discovery = false`.
    fn end_corner_discovery(&mut self, cause: &'static str) {
        if !self.observations.corner_config.discovery {
            return;
        }
        self.observations.corner_config.discovery = false;
        self.observations.discovery_since = None;
        emit_prop_change(
            self,
            DISCOVERY_PATH.into(),
            PropValue::Bool(true),
            PropValue::Bool(false),
            cause,
        );
        if let Some(baseline) = self.observations.watched_baseline.as_mut() {
            baseline.input.corners.discovery = false;
        }
    }

    pub(crate) fn sample_corner_motion(
        &mut self,
        position: (f64, f64),
        region_hint: usize,
        attempted_motion: (f64, f64),
    ) {
        if self.session_lock_active() {
            self.reset_corner_detector();
            return;
        }
        let contains = |region: &CornerRegion| {
            position.0 >= region.origin.0
                && position.0 < region.origin.0 + region.size.0
                && position.1 >= region.origin.1
                && position.1 < region.origin.1 + region.size.1
        };
        let region = self
            .observations
            .corner_regions
            .get(region_hint)
            .copied()
            .filter(contains)
            .or_else(|| {
                self.observations
                    .corner_regions
                    .iter()
                    .copied()
                    .find(contains)
            });
        let Some(region) = region else {
            self.reset_corner_detector();
            return;
        };
        if self.observations.corner_output != Some(region.key_index) {
            self.reset_corner_detector();
            self.observations.corner_output = Some(region.key_index);
            let config = self.observations.corner_config;
            let events = self
                .observations
                .corner_detector
                .reconfigure(config, region.size);
            self.emit_corner_events(events, region.key_index);
        }
        let local = (position.0 - region.origin.0, position.1 - region.origin.1);
        let at_ms =
            u64::try_from(self.observations.corner_clock.elapsed().as_millis()).unwrap_or(u64::MAX);
        let events = self
            .observations
            .corner_detector
            .sample(at_ms, local, attempted_motion);
        self.emit_corner_events(events, region.key_index);
        self.service_corner_presses(position);
        self.rearm_corner_timer();
    }

    pub(crate) fn apply_corner_config(&mut self, config: CornerConfig) {
        let output = self.observations.corner_output;
        let size = output
            .and_then(|index| self.observations.corner_regions.get(index))
            .map_or((0.0, 0.0), |region| region.size);
        if !config.discovery {
            self.observations.discovery_since = None;
        } else if !self.observations.corner_config.discovery
            || self.observations.discovery_since.is_none()
        {
            self.observations.discovery_since = Some(Instant::now());
        }
        self.observations.corner_config = config;
        let events = self.observations.corner_detector.reconfigure(config, size);
        if let Some(output) = output {
            self.emit_corner_events(events, output);
        }
        self.rearm_corner_timer();
        self.publish_hotspots();
    }

    pub(crate) fn reset_corner_detector(&mut self) {
        self.cancel_corner_presses();
        let output = self.observations.corner_output.take();
        let events = self.observations.corner_detector.reset();
        if let Some(output) = output {
            self.emit_corner_events(events, output);
        }
        if let Some(token) = self.observations.corner_timer.take() {
            self.observations.loop_handle.remove(token);
        }
        self.observations.corner_timer_deadline_ms = None;
    }

    fn emit_corner_events(&mut self, events: [Option<CornerEvent>; 2], output_index: usize) {
        if events.iter().all(Option::is_none) {
            return;
        }
        self.emit_corner_event_records(events, output_index);
        // Engagement moved: the hover reveal follows it (§8.7).
        self.publish_hotspots();
    }

    fn emit_corner_event_records(&mut self, events: [Option<CornerEvent>; 2], output_index: usize) {
        for event in events.into_iter().flatten() {
            if matches!(event, CornerEvent::Left { .. }) {
                self.cancel_corner_presses();
            }
            let Some(output) = self
                .observations
                .corner_output_keys
                .get(output_index)
                .cloned()
            else {
                continue;
            };
            match event {
                CornerEvent::Entered { corner, dwell_ms } => {
                    self.break_pointer_constraint_for_corner();
                    self.emit_corner_entered(output, corner, dwell_ms);
                    self.end_corner_discovery("corner.entered");
                }
                CornerEvent::Left { corner, dwell_ms } => {
                    self.emit_corner_left(output, corner, dwell_ms);
                }
            }
        }
    }

    fn rearm_corner_timer(&mut self) {
        let deadline = self.observations.corner_detector.next_deadline_ms();
        if deadline == self.observations.corner_timer_deadline_ms
            && self.observations.corner_timer.is_some()
        {
            return;
        }
        if let Some(token) = self.observations.corner_timer.take() {
            self.observations.loop_handle.remove(token);
        }
        self.observations.corner_timer_deadline_ms = None;
        let Some(deadline_ms) = deadline else {
            return;
        };
        let now_ms =
            u64::try_from(self.observations.corner_clock.elapsed().as_millis()).unwrap_or(u64::MAX);
        let delay = Duration::from_millis(deadline_ms.saturating_sub(now_ms));
        self.observations.corner_timer = self
            .observations
            .loop_handle
            .insert_source(Timer::from_duration(delay), |_, _, state| {
                state.observations.corner_timer = None;
                state.observations.corner_timer_deadline_ms = None;
                let position = state.cursor_position;
                let region_index = state.observations.corner_output.unwrap_or_default();
                state.sample_corner_motion(position, region_index, (0.0, 0.0));
                TimeoutAction::Drop
            })
            .ok();
        if self.observations.corner_timer.is_some() {
            self.observations.corner_timer_deadline_ms = Some(deadline_ms);
            #[cfg(test)]
            {
                self.observations.corner_timer_arms =
                    self.observations.corner_timer_arms.saturating_add(1);
            }
        }
    }

    /// Whether the pointer currently sits inside an engaged Quoin corner —
    /// the corner-supremacy gate `new_constraint` consults before
    /// activating a fresh pointer constraint.
    pub(crate) fn corner_engaged(&self) -> bool {
        self.observations.corner_detector.engaged_corner().is_some()
    }

    /// Arm the deferred constraint re-activation: a constraint was broken
    /// or declined because a corner was engaged; the next physical pointer
    /// motion that lands outside every corner re-activates the pending
    /// constraint. Deliberately NOT driven by `CornerEvent::Left` — the
    /// detector also emits Left on output-geometry resets with the cursor
    /// still physically inside the corner, and activation must follow the
    /// motion delivery it belongs after, not precede it.
    pub(crate) fn defer_constraint_activation(&mut self) {
        self.observations.constraint_activation_deferred = true;
    }

    pub(crate) fn constraint_activation_deferred(&self) -> bool {
        self.observations.constraint_activation_deferred
    }

    pub(crate) fn clear_deferred_constraint_activation(&mut self) {
        self.observations.constraint_activation_deferred = false;
    }

    #[cfg(test)]
    pub(crate) fn corner_timer_probe(&mut self) -> (Option<u64>, usize) {
        self.rearm_corner_timer();
        (
            self.observations.corner_timer_deadline_ms,
            self.observations.corner_timer_arms,
        )
    }

    #[cfg(test)]
    pub(crate) fn corner_candidate_position_probe(&self) -> Option<(f64, f64)> {
        self.observations.corner_detector.candidate_position()
    }
}

pub(super) fn service_observations(state: &mut WaylandState) {
    let stable =
        state.pointer_hit_test_batch_depth == 0 && !state.pointer_hit_test_transaction_applying;
    debug_assert!(
        stable,
        "Bus observation attempted inside a protocol transaction or hit-test batch"
    );
    if !stable {
        return;
    }
    service_panel_holders(state);
    service_surface_edges(state);
    service_focus_edge(state);
    // After the focus edge, which records a closed popup's focus fallback;
    // a restoration is itself a focus change, reported in this cycle.
    if service_popup_restores(state) {
        service_focus_edge(state);
    }
    service_output_edges(state);
    service_property_diffs(state);
    let mutation = service_controls(state);
    let retrack = std::mem::take(&mut state.observations.panel_request_serviced)
        || !matches!(mutation, ControlMutation::None);
    match mutation {
        // A mutation moved state after the edge passes above ran; report it
        // in this cycle rather than on whatever event wakes the loop next.
        // Injected input can only move focus (and the rows that show it).
        ControlMutation::None => {}
        ControlMutation::Input => {
            service_focus_edge(state);
            service_property_diffs(state);
        }
        ControlMutation::Any => {
            service_surface_edges(state);
            service_focus_edge(state);
            service_output_edges(state);
            service_property_diffs(state);
        }
    }
    // Holder requests and Bus-driven focus or input changes were handled
    // after the pass above tracked the holders: reconcile again before the
    // loop sleeps, or a release that leaves a lingering pointer as the last
    // holder (or a mode that retires a deadline) waits for an unrelated event.
    // Enforcement that changes there moves visibility and focus after this
    // cycle's edge passes: report those here too.
    if retrack
        && !state.observations.panel_holders.is_empty()
        && track_panel_holders(state, Instant::now())
    {
        service_surface_edges(state);
        service_focus_edge(state);
        service_property_diffs(state);
    }
    state.service_window_waiters();
    service_pointer(state);
}

/// Runs at a stable post-dispatch boundary. Input handlers never serialize or
/// wait for the Bus; only the latest authoritative cursor snapshot is read.
fn service_pointer(state: &mut WaylandState) {
    let now = Instant::now();
    if state.observations.pointer_lease.active(now) {
        let cursor = *state
            .cursor_position_snapshot
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let suppressed = state.session_lock_active() || !state.backend.pointer_session_active();
        if state.observations.pointer_seen != Some((cursor, suppressed)) {
            state.observations.pointer_seen = Some((cursor, suppressed));
            state.observations.pointer_lease.changed();
        }
        if state.observations.pointer_lease.take(now)
            && let Some(context) = state.port_context.as_ref()
        {
            let instance = context.instance.clone();
            let position = if !suppressed
                && cursor.on_output
                && cursor.x.is_finite()
                && cursor.y.is_finite()
            {
                project_outputs(state).and_then(|projection| {
                    projection.rows.values().find_map(|row| {
                        let x = cursor.x - f64::from(row.x);
                        let y = cursor.y - f64::from(row.y);
                        (x >= 0.0
                            && y >= 0.0
                            && x < f64::from(row.width)
                            && y < f64::from(row.height))
                        .then(|| (row.name.clone(), PointerPosition { x, y }))
                    })
                })
            } else {
                None
            };
            let valid = position.is_some();
            let (output, position) = position.map_or((None, None), |(output, position)| {
                (Some(output), Some(position))
            });
            let sample = PointerSample {
                version: 1,
                instance,
                output,
                position,
                valid,
                timestamp_ms: state
                    .observations
                    .corner_clock
                    .elapsed()
                    .as_millis()
                    .min(u128::from(u64::MAX)) as u64,
            };
            state
                .observations
                .offer(|event_seq| ObservationRecord::PointerChanged { sample, event_seq });
        }
    } else {
        state.observations.pointer_seen = None;
    }
    let deadline = state.observations.pointer_lease.deadline(now);
    if deadline == state.observations.pointer_deadline {
        return;
    }
    if let Some(token) = state.observations.pointer_timer.take() {
        state.observations.loop_handle.remove(token);
    }
    state.observations.pointer_deadline = None;
    if let Some(deadline) = deadline {
        let delay = deadline.saturating_duration_since(Instant::now());
        let timer = state.observations.loop_handle.insert_source(
            Timer::from_duration(delay),
            |_, _, state| {
                state.observations.pointer_timer = None;
                state.observations.pointer_deadline = None;
                // The normal dispatch-cycle epilogue services the due sample.
                TimeoutAction::Drop
            },
        );
        match timer {
            Ok(token) => {
                state.observations.pointer_timer = Some(token);
                state.observations.pointer_deadline = Some(deadline);
            }
            Err(error) => tracing::warn!(%error, "pointer observation timer unavailable"),
        }
    }
}

fn edge_window(state: &WaylandState, record: &super::SurfaceRecord) -> SurfaceEdgeWindow {
    let redact = state.session_lock_active();
    SurfaceEdgeWindow {
        generation: record.generation,
        app_id: (!redact).then(|| record.app_id.clone()).flatten(),
        title: (!redact).then(|| record.title.clone()).flatten(),
    }
}

fn service_surface_edges(state: &mut WaylandState) {
    let pending = std::mem::take(&mut state.observations.pending_surface_edges);
    for (raw_id, old) in pending {
        let id = SurfaceId(raw_id);
        let final_record = state
            .surface_objects
            .get(&id)
            .and_then(|object| state.surfaces.get(object));
        let final_mapped = final_record.is_some_and(|record| record.mapped);
        // An unmap and a remap under a new role inside one cycle is still
        // two edges: the window observers knew is gone.
        let replaced = old.mapped
            && final_mapped
            && final_record.is_some_and(|record| record.generation != old.window.generation);
        if old.mapped == final_mapped && !replaced {
            continue;
        }
        let previous =
            replaced.then(|| (old.role.clone(), old.foreign_id.clone(), old.window.clone()));
        let role = if final_mapped {
            final_record
                .map(|record| record.role.kind().to_string())
                .unwrap_or(old.role)
        } else {
            old.role
        };
        let foreign_id = state
            .foreign_toplevel_identifiers
            .get(&id)
            .cloned()
            .or(old.foreign_id);
        let window = match final_record {
            Some(record) if final_mapped => edge_window(state, record),
            _ => old.window,
        };
        if let Some((role, foreign_id, window)) = previous {
            state
                .observations
                .offer(|event_seq| ObservationRecord::SurfaceUnmapped {
                    id: raw_id,
                    role,
                    foreign_id,
                    window,
                    event_seq,
                });
        }
        state.observations.offer(|event_seq| {
            if final_mapped {
                ObservationRecord::SurfaceMapped {
                    id: raw_id,
                    role,
                    foreign_id,
                    window,
                    event_seq,
                }
            } else {
                ObservationRecord::SurfaceUnmapped {
                    id: raw_id,
                    role,
                    foreign_id,
                    window,
                    event_seq,
                }
            }
        });
    }
}

fn service_focus_edge(state: &mut WaylandState) {
    let Some(previous) = state.observations.pending_focus.take() else {
        return;
    };
    let current = project_focus_edge(state);
    if previous.keyboard == current.keyboard && previous.exclusive_latch == current.exclusive_latch
    {
        return;
    }
    if let Some(id) = previous.keyboard {
        state.mark_surface_dirty(SurfaceId(id), "wayland.focus");
    }
    if let Some(id) = current.keyboard {
        state.mark_surface_dirty(SurfaceId(id), "wayland.focus");
    }
    state.observations.last_focus_change = Some((current.keyboard, previous.keyboard));
    note_popup_focus(state, current.keyboard, previous.keyboard);
    state
        .observations
        .offer(|event_seq| ObservationRecord::FocusChanged {
            keyboard: current.keyboard,
            previous: previous.keyboard,
            exclusive_latch: current.exclusive_latch,
            event_seq,
        });
}

fn project_focus_edge(state: &WaylandState) -> FocusEdgeStart {
    FocusEdgeStart {
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
    }
}

fn service_output_edges(state: &mut WaylandState) {
    let pending = std::mem::take(&mut state.observations.dirty_outputs);
    let topology_dirty = std::mem::take(&mut state.observations.output_topology_dirty);
    if pending.is_empty() && !topology_dirty {
        return;
    }
    let final_rows = if topology_dirty {
        let Some(projection) = project_outputs(state) else {
            state.observations.dirty_outputs = pending;
            state.observations.output_topology_dirty = true;
            if state.observations.watched_baseline.is_some() {
                state
                    .observations
                    .full_dirty
                    .get_or_insert("output.geometry");
            }
            return;
        };
        projection.rows
    } else {
        BTreeMap::new()
    };
    let old_keys = pending.keys().cloned().collect::<BTreeSet<_>>();
    let final_keys = final_rows.keys().cloned().collect::<BTreeSet<_>>();
    let topology_replaced = topology_dirty && old_keys != final_keys;
    if topology_replaced {
        state.reset_corner_detector();
    }
    let mut emitted = BTreeSet::new();
    let mut retry = BTreeMap::new();
    for (key, old) in pending {
        let row = if topology_dirty {
            final_rows.get(&key).cloned()
        } else {
            project_output(state, &old.output)
                .and_then(|(final_key, row)| (final_key == key).then_some(row))
        };
        let Some(row) = row else {
            if state.backend.port_output(&old.output).is_some() {
                retry.insert(key, old);
            }
            continue;
        };
        if old.row.x == row.x
            && old.row.y == row.y
            && old.row.width == row.width
            && old.row.height == row.height
            && old.row.usable == row.usable
        {
            continue;
        }
        state.reset_corner_detector();
        state
            .observations
            .offer(|event_seq| ObservationRecord::OutputChanged {
                output: key.clone(),
                row,
                event_seq,
            });
        emitted.insert(key);
    }
    state.observations.dirty_outputs.extend(retry);
    if topology_replaced {
        for (key, row) in final_rows {
            if old_keys.contains(&key) || emitted.contains(&key) {
                continue;
            }
            state.reset_corner_detector();
            state
                .observations
                .offer(|event_seq| ObservationRecord::OutputChanged {
                    output: key,
                    row,
                    event_seq,
                });
        }
    }
    state.refresh_corner_regions();
}

fn service_property_diffs(state: &mut WaylandState) {
    let Some(mut baseline) = state.observations.watched_baseline.take() else {
        state.observations.dirty_surfaces.clear();
        state.observations.property_dirty_outputs.clear();
        state.observations.stack_dirty = None;
        state.observations.full_dirty = None;
        state.observations.focus_cause = "wayland.focus";
        return;
    };
    let mut changes = PendingPropChanges::new();
    let full_cause = state.observations.full_dirty.take();
    if let Some(cause) = full_cause {
        let Some(context) = state.port_context.clone() else {
            state.observations.full_dirty = Some(cause);
            state.observations.watched_baseline = Some(baseline);
            return;
        };
        let Some(next) = snapshot(state, &context) else {
            state.observations.full_dirty = Some(cause);
            state.observations.watched_baseline = Some(baseline);
            return;
        };
        collect_snapshot_diff(&baseline, &next, cause, &mut changes);
        baseline = next;
        state.observations.dirty_surfaces.clear();
        state.observations.property_dirty_outputs.clear();
        state.observations.stack_dirty = None;
        state.observations.focus_cause = "wayland.focus";
        flush_prop_changes(state, changes);
        state.observations.watched_baseline = Some(baseline);
        return;
    }

    let dirty_outputs = std::mem::take(&mut state.observations.property_dirty_outputs);
    for (key, (output, cause)) in dirty_outputs {
        let old = baseline.outputs.get(&key).cloned();
        let new = project_output(state, &output)
            .filter(|(final_key, _)| final_key == &key)
            .map(|(_, row)| row);
        if new.is_none() && state.backend.port_output(&output).is_some() {
            state.observations.full_dirty.get_or_insert(cause);
            continue;
        }
        diff_output_row(
            &format!("outputs.{key}"),
            old.as_ref(),
            new.as_ref(),
            cause,
            &mut changes,
        );
        match new {
            Some(row) => {
                baseline.outputs.insert(key, row);
            }
            None => {
                baseline.outputs.remove(&key);
            }
        }
    }
    let dirty_surfaces = std::mem::take(&mut state.observations.dirty_surfaces);
    if !dirty_surfaces.is_empty() {
        if let Some(projection) = project_outputs(state) {
            for (raw_id, cause) in dirty_surfaces {
                let key = format!("s{raw_id}");
                let old_surface = baseline.surfaces.get(&key).cloned();
                let new_surface = project_surface_by_id(state, SurfaceId(raw_id), &projection.keys);
                diff_surface_row(
                    &format!("surfaces.{key}"),
                    old_surface.as_ref(),
                    new_surface.as_ref(),
                    cause,
                    &mut changes,
                );
                match new_surface {
                    Some(row) => {
                        baseline.surfaces.insert(key.clone(), row.clone());
                        if row.role == "toplevel" && row.mapped && !state.session_lock_active() {
                            let old_window = baseline.windows.get(&key).cloned();
                            let new_window = project_window_row(&row);
                            diff_window_row(
                                &format!("windows.{key}"),
                                old_window.as_ref(),
                                Some(&new_window),
                                cause,
                                &mut changes,
                            );
                            baseline.windows.insert(key, new_window);
                        } else if let Some(old_window) = baseline.windows.remove(&key) {
                            diff_window_row(
                                &format!("windows.{key}"),
                                Some(&old_window),
                                None,
                                cause,
                                &mut changes,
                            );
                        }
                    }
                    None => {
                        baseline.surfaces.remove(&key);
                        if let Some(old_window) = baseline.windows.remove(&key) {
                            diff_window_row(
                                &format!("windows.{key}"),
                                Some(&old_window),
                                None,
                                cause,
                                &mut changes,
                            );
                        }
                    }
                }
            }
        } else if let Some(cause) = dirty_surfaces.values().next().copied() {
            state.observations.full_dirty.get_or_insert(cause);
        }
    }

    if let Some(cause) = state.observations.stack_dirty.take() {
        let next = project_stack(state);
        if baseline.stack != next {
            queue_prop_change(
                &mut changes,
                "stack".to_string(),
                PropValue::U64List(baseline.stack.clone()),
                PropValue::U64List(next.clone()),
                cause,
            );
            baseline.stack = next;
        }
    }
    let focus = project_focus(state);
    if baseline.focus != focus {
        let cause = state.observations.focus_cause;
        diff_focus("focus", &baseline.focus, &focus, cause, &mut changes);
        baseline.focus = focus;
    }
    state.observations.focus_cause = "wayland.focus";
    flush_prop_changes(state, changes);
    state.observations.watched_baseline = Some(baseline);
}

fn collect_snapshot_diff(
    old: &CompSnapshot,
    new: &CompSnapshot,
    cause: &'static str,
    pending: &mut PendingPropChanges,
) {
    let output_keys = old
        .outputs
        .keys()
        .chain(new.outputs.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    for key in output_keys {
        diff_output_row(
            &format!("outputs.{key}"),
            old.outputs.get(&key),
            new.outputs.get(&key),
            cause,
            pending,
        );
    }
    let surface_keys = old
        .surfaces
        .keys()
        .chain(new.surfaces.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    for key in surface_keys {
        diff_surface_row(
            &format!("surfaces.{key}"),
            old.surfaces.get(&key),
            new.surfaces.get(&key),
            cause,
            pending,
        );
    }
    let window_keys = old
        .windows
        .keys()
        .chain(new.windows.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    for key in window_keys {
        diff_window_row(
            &format!("windows.{key}"),
            old.windows.get(&key),
            new.windows.get(&key),
            cause,
            pending,
        );
    }
    queue_prop_change(
        pending,
        "stack".into(),
        PropValue::U64List(old.stack.clone()),
        PropValue::U64List(new.stack.clone()),
        cause,
    );
    diff_focus("focus", &old.focus, &new.focus, cause, pending);
    queue_prop_change(
        pending,
        "decoration.enabled".into(),
        PropValue::Bool(old.decoration.enabled),
        PropValue::Bool(new.decoration.enabled),
        cause,
    );
    queue_prop_change(
        pending,
        "decoration.style".into(),
        prop_str(old.decoration.style),
        prop_str(new.decoration.style),
        cause,
    );
    queue_prop_change(
        pending,
        "bindings.enabled".into(),
        PropValue::Bool(old.bindings.enabled),
        PropValue::Bool(new.bindings.enabled),
        cause,
    );
    queue_prop_change(
        pending,
        "bindings.profile".into(),
        prop_str(old.bindings.profile),
        prop_str(new.bindings.profile),
        cause,
    );
    queue_prop_change(
        pending,
        "bindings.table".into(),
        PropValue::BindingRows(old.bindings.table.clone()),
        PropValue::BindingRows(new.bindings.table.clone()),
        cause,
    );
    diff_corners(old, new, cause, pending);
    diff_workspaces(old, new, cause, pending);
    #[cfg(feature = "xwayland")]
    queue_prop_change(
        pending,
        "xwayland.display".into(),
        prop_opt_string(old.xwayland.display.as_deref()),
        prop_opt_string(new.xwayland.display.as_deref()),
        cause,
    );
}

/// `workspaces.*`: the scalar leaves, one `o_<key>.current` per output in
/// either snapshot (an output that left reads null, like an output row),
/// and the row list as one value (the `bindings.table` precedent).
fn diff_workspaces(
    old: &CompSnapshot,
    new: &CompSnapshot,
    cause: &'static str,
    pending: &mut PendingPropChanges,
) {
    let (old, new) = (&old.workspaces, &new.workspaces);
    queue_prop_change(
        pending,
        "workspaces.count".into(),
        PropValue::U32(old.count),
        PropValue::U32(new.count),
        cause,
    );
    queue_prop_change(
        pending,
        "workspaces.current".into(),
        PropValue::U32(old.current),
        PropValue::U32(new.current),
        cause,
    );
    let keys = old
        .outputs
        .keys()
        .chain(new.outputs.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    for key in keys {
        let current = |row: Option<&super::port_snapshot::OutputWorkspaceSnapshot>| {
            row.map_or_else(PropValue::null, |row| PropValue::U32(row.current))
        };
        queue_prop_change(
            pending,
            format!("workspaces.{key}.current"),
            current(old.outputs.get(&key)),
            current(new.outputs.get(&key)),
            cause,
        );
    }
    queue_prop_change(
        pending,
        "workspaces.list".into(),
        PropValue::WorkspaceRows(old.list.clone()),
        PropValue::WorkspaceRows(new.list.clone()),
        cause,
    );
}

fn diff_corners(
    old: &CompSnapshot,
    new: &CompSnapshot,
    cause: &'static str,
    pending: &mut PendingPropChanges,
) {
    let (old_host, new_host) = (old.input.host, new.input.host);
    let old = old.input.corners;
    let new = new.input.corners;
    for (leaf, old, new) in [
        (
            "enabled",
            PropValue::Bool(old.enabled),
            PropValue::Bool(new.enabled),
        ),
        (
            "deadzone_px",
            PropValue::F64(old.deadzone_px),
            PropValue::F64(new.deadzone_px),
        ),
        (
            "dwell_ms",
            PropValue::U64(old.dwell_ms),
            PropValue::U64(new.dwell_ms),
        ),
        (
            "velocity_max_px_s",
            PropValue::F64(old.velocity_max_px_s),
            PropValue::F64(new.velocity_max_px_s),
        ),
        (
            "affordance",
            PropValue::Bool(old.affordance),
            PropValue::Bool(new.affordance),
        ),
        (
            "discovery",
            PropValue::Bool(old.discovery),
            PropValue::Bool(new.discovery),
        ),
    ] {
        queue_prop_change(pending, format!("input.corners.{leaf}"), old, new, cause);
    }
    if let (Some(old), Some(new)) = (old_host, new_host) {
        queue_prop_change(
            pending,
            HOST_PASSTHROUGH_PATH.into(),
            PropValue::Bool(old.passthrough),
            PropValue::Bool(new.passthrough),
            cause,
        );
    }
}

fn diff_output_row(
    prefix: &str,
    old: Option<&OutputSnapshot>,
    new: Option<&OutputSnapshot>,
    cause: &'static str,
    pending: &mut PendingPropChanges,
) {
    match (old, new) {
        (None, None) => {}
        (None, Some(new)) => {
            queue_prop_change(
                pending,
                prefix.into(),
                PropValue::null(),
                PropValue::OutputRow(Box::new(OutputSnapshot {
                    presentation: None,
                    ..new.clone()
                })),
                cause,
            );
        }
        (Some(old), None) => {
            queue_prop_change(
                pending,
                prefix.into(),
                PropValue::OutputRow(Box::new(OutputSnapshot {
                    presentation: None,
                    ..old.clone()
                })),
                PropValue::null(),
                cause,
            );
        }
        (Some(old), Some(new)) => {
            for (leaf, old, new) in [
                ("name", prop_str(&old.name), prop_str(&new.name)),
                (
                    "default",
                    PropValue::Bool(old.default),
                    PropValue::Bool(new.default),
                ),
                ("x", PropValue::I32(old.x), PropValue::I32(new.x)),
                ("y", PropValue::I32(old.y), PropValue::I32(new.y)),
                (
                    "width",
                    PropValue::U32(old.width),
                    PropValue::U32(new.width),
                ),
                (
                    "height",
                    PropValue::U32(old.height),
                    PropValue::U32(new.height),
                ),
                (
                    "scale",
                    PropValue::F64(old.scale),
                    PropValue::F64(new.scale),
                ),
                (
                    "refresh_mhz",
                    PropValue::U32(old.refresh_mhz),
                    PropValue::U32(new.refresh_mhz),
                ),
                (
                    "usable.x",
                    PropValue::F32(old.usable.x),
                    PropValue::F32(new.usable.x),
                ),
                (
                    "usable.y",
                    PropValue::F32(old.usable.y),
                    PropValue::F32(new.usable.y),
                ),
                (
                    "usable.width",
                    PropValue::F32(old.usable.width),
                    PropValue::F32(new.usable.width),
                ),
                (
                    "usable.height",
                    PropValue::F32(old.usable.height),
                    PropValue::F32(new.usable.height),
                ),
            ] {
                queue_prop_change(pending, format!("{prefix}.{leaf}"), old, new, cause);
            }
        }
    }
}

fn diff_occlusion(
    prefix: &str,
    old: &crate::occlusion::Props,
    new: &crate::occlusion::Props,
    cause: &'static str,
    pending: &mut PendingPropChanges,
) {
    for (leaf, old, new) in [
        (
            "occluded",
            PropValue::Bool(old.occluded),
            PropValue::Bool(new.occluded),
        ),
        (
            "occlusion_reason",
            prop_str(old.occlusion_reason),
            prop_str(new.occlusion_reason),
        ),
        (
            "occlusion_revision",
            PropValue::U64(old.occlusion_revision),
            PropValue::U64(new.occlusion_revision),
        ),
    ] {
        queue_prop_change(pending, format!("{prefix}.{leaf}"), old, new, cause);
    }
}

#[test]
fn occlusion_counters_never_emit_changes_but_decisions_do() {
    let old = crate::occlusion::Props::default();
    let mut new = old.clone();
    let mut changes = PendingPropChanges::new();
    queue_prop_change(
        &mut changes,
        "occlusion.counters.recomputes".into(),
        PropValue::U64(0),
        PropValue::U64(42),
        "wayland.occlusion",
    );
    diff_occlusion("surfaces.s1", &old, &new, "wayland.occlusion", &mut changes);
    assert!(changes.is_empty());
    new.occluded = true;
    new.occlusion_reason = "opaque-coverage";
    new.occlusion_revision = 2;
    diff_occlusion("surfaces.s1", &old, &new, "wayland.occlusion", &mut changes);
    assert_eq!(changes.len(), 3);
    assert!(changes.contains_key("surfaces.s1.occluded"));
}

fn diff_surface_row(
    prefix: &str,
    old: Option<&SurfaceSnapshot>,
    new: Option<&SurfaceSnapshot>,
    cause: &'static str,
    pending: &mut PendingPropChanges,
) {
    if let (Some(old), Some(new)) = (old, new) {
        diff_occlusion(prefix, &old.occlusion, &new.occlusion, cause, pending);
    }
    let (old, new) = match (old, new) {
        (None, None) => return,
        (None, Some(new)) => {
            let new = new.clone();
            queue_prop_change(
                pending,
                prefix.into(),
                PropValue::null(),
                PropValue::SurfaceRow(Box::new(new)),
                cause,
            );
            return;
        }
        (Some(old), None) => {
            let old = old.clone();
            queue_prop_change(
                pending,
                prefix.into(),
                PropValue::SurfaceRow(Box::new(old)),
                PropValue::null(),
                cause,
            );
            return;
        }
        (Some(old), Some(new)) => (old, new),
    };
    for (leaf, old, new) in [
        ("id", PropValue::U64(old.id), PropValue::U64(new.id)),
        ("role", prop_str(old.role), prop_str(new.role)),
        (
            "mapped",
            PropValue::Bool(old.mapped),
            PropValue::Bool(new.mapped),
        ),
        (
            "visible",
            PropValue::Bool(old.visible),
            PropValue::Bool(new.visible),
        ),
        ("x", PropValue::F32(old.x), PropValue::F32(new.x)),
        ("y", PropValue::F32(old.y), PropValue::F32(new.y)),
        (
            "width",
            PropValue::F32(old.width),
            PropValue::F32(new.width),
        ),
        (
            "height",
            PropValue::F32(old.height),
            PropValue::F32(new.height),
        ),
        ("band", prop_str(old.band), prop_str(new.band)),
        (
            "sequence",
            PropValue::U64(old.sequence),
            PropValue::U64(new.sequence),
        ),
        (
            "tree_index",
            PropValue::U32(old.tree_index),
            PropValue::U32(new.tree_index),
        ),
        ("parent", prop_opt_u64(old.parent), prop_opt_u64(new.parent)),
        (
            "output",
            prop_opt_string(old.output.as_deref()),
            prop_opt_string(new.output.as_deref()),
        ),
        (
            "title",
            prop_opt_string(old.title.as_deref()),
            prop_opt_string(new.title.as_deref()),
        ),
        (
            "app_id",
            prop_opt_string(old.app_id.as_deref()),
            prop_opt_string(new.app_id.as_deref()),
        ),
        (
            "focused",
            PropValue::Bool(old.focused),
            PropValue::Bool(new.focused),
        ),
        (
            "activated",
            PropValue::Bool(old.activated),
            PropValue::Bool(new.activated),
        ),
        (
            "maximized",
            PropValue::Bool(old.maximized),
            PropValue::Bool(new.maximized),
        ),
        (
            "fullscreen",
            PropValue::Bool(old.fullscreen),
            PropValue::Bool(new.fullscreen),
        ),
        (
            "minimized",
            PropValue::Bool(old.minimized),
            PropValue::Bool(new.minimized),
        ),
        (
            "workspace",
            prop_opt_u32(old.workspace),
            prop_opt_u32(new.workspace),
        ),
        (
            "decoration",
            prop_opt_string(old.decoration),
            prop_opt_string(new.decoration),
        ),
        (
            "foreign_id",
            prop_opt_string(old.foreign_id.as_deref()),
            prop_opt_string(new.foreign_id.as_deref()),
        ),
        (
            "generation",
            PropValue::U64(old.generation),
            PropValue::U64(new.generation),
        ),
    ] {
        queue_prop_change(pending, format!("{prefix}.{leaf}"), old, new, cause);
    }
    diff_layer(
        prefix,
        old.layer.as_ref(),
        new.layer.as_ref(),
        cause,
        pending,
    );
}

fn diff_layer(
    prefix: &str,
    old: Option<&LayerSnapshot>,
    new: Option<&LayerSnapshot>,
    cause: &'static str,
    pending: &mut PendingPropChanges,
) {
    let old_stratum = old.map_or_else(PropValue::null, |row| prop_str(row.stratum));
    let new_stratum = new.map_or_else(PropValue::null, |row| prop_str(row.stratum));
    let old_interactivity = old.map_or_else(PropValue::null, |row| prop_str(row.interactivity));
    let new_interactivity = new.map_or_else(PropValue::null, |row| prop_str(row.interactivity));
    let old_zone = old.map_or_else(PropValue::null, |row| PropValue::I32(row.exclusive_zone));
    let new_zone = new.map_or_else(PropValue::null, |row| PropValue::I32(row.exclusive_zone));
    let old_binding = old.map_or_else(PropValue::null, |row| prop_str(row.binding));
    let new_binding = new.map_or_else(PropValue::null, |row| prop_str(row.binding));
    for (leaf, old, new) in [
        ("stratum", old_stratum, new_stratum),
        ("interactivity", old_interactivity, new_interactivity),
        ("exclusive_zone", old_zone, new_zone),
        ("binding", old_binding, new_binding),
    ] {
        queue_prop_change(pending, format!("{prefix}.layer.{leaf}"), old, new, cause);
    }
}

fn diff_window_row(
    prefix: &str,
    old: Option<&WindowSnapshot>,
    new: Option<&WindowSnapshot>,
    cause: &'static str,
    pending: &mut PendingPropChanges,
) {
    if let (Some(old), Some(new)) = (old, new) {
        diff_occlusion(prefix, &old.occlusion, &new.occlusion, cause, pending);
    }
    let (old, new) = match (old, new) {
        (None, None) => return,
        (None, Some(new)) => {
            queue_prop_change(
                pending,
                prefix.into(),
                PropValue::null(),
                PropValue::WindowRow(Box::new(WindowSnapshot {
                    presentation: None,
                    ..new.clone()
                })),
                cause,
            );
            return;
        }
        (Some(old), None) => {
            queue_prop_change(
                pending,
                prefix.into(),
                PropValue::WindowRow(Box::new(WindowSnapshot {
                    presentation: None,
                    ..old.clone()
                })),
                PropValue::null(),
                cause,
            );
            return;
        }
        (Some(old), Some(new)) => (old, new),
    };
    for (leaf, old, new) in [
        ("id", PropValue::U64(old.id), PropValue::U64(new.id)),
        (
            "foreign_id",
            prop_opt_string(old.foreign_id.as_deref()),
            prop_opt_string(new.foreign_id.as_deref()),
        ),
        (
            "title",
            prop_opt_string(old.title.as_deref()),
            prop_opt_string(new.title.as_deref()),
        ),
        (
            "app_id",
            prop_opt_string(old.app_id.as_deref()),
            prop_opt_string(new.app_id.as_deref()),
        ),
        ("x", PropValue::F32(old.x), PropValue::F32(new.x)),
        ("y", PropValue::F32(old.y), PropValue::F32(new.y)),
        (
            "width",
            PropValue::F32(old.width),
            PropValue::F32(new.width),
        ),
        (
            "height",
            PropValue::F32(old.height),
            PropValue::F32(new.height),
        ),
        (
            "focused",
            PropValue::Bool(old.focused),
            PropValue::Bool(new.focused),
        ),
        (
            "maximized",
            PropValue::Bool(old.maximized),
            PropValue::Bool(new.maximized),
        ),
        (
            "fullscreen",
            PropValue::Bool(old.fullscreen),
            PropValue::Bool(new.fullscreen),
        ),
        (
            "minimized",
            PropValue::Bool(old.minimized),
            PropValue::Bool(new.minimized),
        ),
        (
            "output",
            prop_opt_string(old.output.as_deref()),
            prop_opt_string(new.output.as_deref()),
        ),
        (
            "band",
            PropValue::String(old.band.into()),
            PropValue::String(new.band.into()),
        ),
        (
            "generation",
            PropValue::U64(old.generation),
            PropValue::U64(new.generation),
        ),
        (
            "window_x",
            PropValue::F32(old.window_x),
            PropValue::F32(new.window_x),
        ),
        (
            "window_y",
            PropValue::F32(old.window_y),
            PropValue::F32(new.window_y),
        ),
        (
            "window_width",
            PropValue::F32(old.window_width),
            PropValue::F32(new.window_width),
        ),
        (
            "window_height",
            PropValue::F32(old.window_height),
            PropValue::F32(new.window_height),
        ),
        (
            "visible",
            PropValue::Bool(old.visible),
            PropValue::Bool(new.visible),
        ),
        ("pid", prop_opt_u64(old.pid), prop_opt_u64(new.pid)),
        (
            "workspace",
            PropValue::U32(old.workspace),
            PropValue::U32(new.workspace),
        ),
    ] {
        queue_prop_change(pending, format!("{prefix}.{leaf}"), old, new, cause);
    }
}

fn diff_focus(
    prefix: &str,
    old: &FocusSnapshot,
    new: &FocusSnapshot,
    cause: &'static str,
    pending: &mut PendingPropChanges,
) {
    for (leaf, old, new) in [
        (
            "keyboard",
            prop_opt_u64(old.keyboard),
            prop_opt_u64(new.keyboard),
        ),
        (
            "exclusive_latch",
            prop_opt_u64(old.exclusive_latch),
            prop_opt_u64(new.exclusive_latch),
        ),
        (
            "pointer",
            prop_opt_u64(old.pointer),
            prop_opt_u64(new.pointer),
        ),
        (
            "pointer_grab",
            prop_str(old.pointer_grab),
            prop_str(new.pointer_grab),
        ),
        (
            "session_lock",
            prop_str(old.session_lock),
            prop_str(new.session_lock),
        ),
        (
            "window.id",
            prop_opt_u64(old.window.id),
            prop_opt_u64(new.window.id),
        ),
        (
            "window.generation",
            prop_opt_u64(old.window.generation),
            prop_opt_u64(new.window.generation),
        ),
    ] {
        queue_prop_change(pending, format!("{prefix}.{leaf}"), old, new, cause);
    }
}

fn prop_str(value: &str) -> PropValue {
    PropValue::String(value.to_string())
}

fn prop_opt_string(value: Option<&str>) -> PropValue {
    value.map_or_else(PropValue::null, prop_str)
}

fn prop_opt_u64(value: Option<u64>) -> PropValue {
    value.map_or_else(PropValue::null, PropValue::U64)
}

fn prop_opt_u32(value: Option<u32>) -> PropValue {
    value.map_or_else(PropValue::null, PropValue::U32)
}

fn queue_prop_change(
    pending: &mut PendingPropChanges,
    path: String,
    old: PropValue,
    new: PropValue,
    cause: &'static str,
) {
    if old == new || path.starts_with("port.") || volatile_path(&path) {
        return;
    }
    match pending.entry(path) {
        std::collections::btree_map::Entry::Vacant(entry) => {
            entry.insert((old, new, cause));
        }
        std::collections::btree_map::Entry::Occupied(mut entry) => {
            entry.get_mut().1 = new;
            if entry.get().0 == entry.get().1 {
                entry.remove();
            }
        }
    }
}

fn flush_prop_changes(state: &mut WaylandState, changes: PendingPropChanges) {
    for (path, (old, new, cause)) in changes {
        emit_prop_change(state, path, old, new, cause);
    }
}

fn emit_prop_change(
    state: &mut WaylandState,
    path: String,
    old: PropValue,
    new: PropValue,
    cause: &'static str,
) {
    if old == new || path.starts_with("port.") || volatile_path(&path) {
        return;
    }
    let unix_ms = unix_millis();
    state
        .observations
        .offer(|event_seq| ObservationRecord::PropsChanged {
            path,
            old,
            new,
            unix_ms,
            cause,
            event_seq,
        });
}

fn unix_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
        })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum ControlMutation {
    None,
    Input,
    Any,
}

/// Returns the widest kind of state change the controls made.
fn service_controls(state: &mut WaylandState) -> ControlMutation {
    let mut controls = std::mem::take(&mut state.pending_port_controls);
    if let Some(context) = state.port_context.as_ref() {
        for (active, order) in [
            (false, context.pending_idle_order.swap(0, Ordering::AcqRel)),
            (true, context.pending_active_order.swap(0, Ordering::AcqRel)),
        ] {
            if order != 0 {
                controls.push(PortControl::WatchState { active, order });
            }
        }
    }
    controls.sort_by_key(PortControl::order);
    let mut changes = PendingPropChanges::new();
    let mut mutated = ControlMutation::None;
    // Mutations run in arrival order, so a script's set -> minimise ->
    // restore -> click lands in the order it was sent.
    for control in &mut controls {
        match control {
            PortControl::Panel(request) => {
                state.observations.panel_request_serviced = true;
                let reply = service_panel_request(state, &request.op);
                if let Some(sender) = request.reply.take() {
                    let _ = sender.send(reply);
                }
            }
            PortControl::Set(request) => {
                mutated = ControlMutation::Any;
                service_set(state, request, &mut changes);
            }
            PortControl::Window(request) => {
                mutated = ControlMutation::Any;
                let reply = state.service_window_op(&request.op);
                if let Some(sender) = request.reply.take() {
                    let _ = sender.send(reply);
                }
            }
            PortControl::Input(request) => {
                mutated = mutated.max(ControlMutation::Input);
                let reply = state.service_input_op(&request.op);
                if let Some(sender) = request.reply.take() {
                    let _ = sender.send(reply);
                }
            }
            PortControl::Long(request) => {
                mutated = ControlMutation::Any;
                // The ingress slot is released here: the verb now waits on
                // its own permit and deadline, not on the bounded queue.
                request.slot.take();
                if let (Some(op), Some(reply)) = (request.op.take(), request.reply.take()) {
                    state.start_long_op(op, reply, request.admitted);
                }
            }
            PortControl::Watch(_)
            | PortControl::PointerWatch(_)
            | PortControl::WatchState { .. } => {}
        }
    }
    flush_set_changes(state, changes);

    let mut desired_active = state.observations.watched_baseline.is_some();
    let mut watches = Vec::new();
    for control in controls {
        match control {
            PortControl::PointerWatch(request) => {
                state.observations.pointer_lease.renew(Instant::now());
                let reply = state
                    .port_context
                    .as_ref()
                    .map_or(ControlReply::Busy, |context| ControlReply::PointerWatch {
                        topic: topic_name(&context.service, POINTER_TOPIC_SUFFIX),
                        lease_ms: LEASE.as_millis() as u64,
                    });
                let _ = request.reply.send(reply);
            }
            PortControl::Watch(request) => {
                desired_active = true;
                watches.push(request);
            }
            PortControl::WatchState { active, .. } => desired_active = active,
            PortControl::Set(_)
            | PortControl::Panel(_)
            | PortControl::Window(_)
            | PortControl::Input(_)
            | PortControl::Long(_) => {}
        }
    }

    let needs_seed =
        !watches.is_empty() || (desired_active && state.observations.watched_baseline.is_none());
    let seed = needs_seed
        .then(|| {
            state
                .port_context
                .clone()
                .and_then(|context| snapshot(state, &context).map(|baseline| (context, baseline)))
        })
        .flatten();
    if desired_active {
        if let Some((_, baseline)) = &seed {
            state.observations.watched_baseline = Some(baseline.clone());
        }
    } else {
        state.observations.drop_watch();
    }
    for request in watches {
        let reply = seed
            .as_ref()
            .map_or(ControlReply::Busy, |(context, _)| ControlReply::Watch {
                topic: topic_name(&context.service, PROPS_TOPIC_SUFFIX),
                event_seq: state.observations.event_seq,
                lost_count: context.lost_count.load(Ordering::Acquire),
            });
        let _ = request.reply.send(reply);
    }
    mutated
}

/// Holder state follows the layers and outputs it names, and comp tracks the
/// pointer and focus holders itself (shell design §4.3). Membership moves with
/// the pointer, so this runs at every stable dispatch boundary while Quoin has
/// reported a panel — a few lookups per edge — but a command is emitted only
/// when an edge's verdict changes, and the only timer is one one-shot: the
/// conceal deadline (armed when a lingering pointer becomes the last holder,
/// cancelled when any holder returns) or the enforcement grace after a
/// conceal (shell design §7: a stalled Quoin cannot keep a panel shown or
/// taking input), whichever is earlier. Nothing polls.
fn service_panel_holders(state: &mut WaylandState) {
    if state.observations.panel_holders.is_empty() {
        rearm_conceal_timer(state, None);
        apply_panel_enforcement(state);
        return;
    }
    // Output removal drops that output's modes and holds without a signal to
    // Quoin. None is needed: the removal destroys the wl_output, the Quoin
    // host rebuilds every panel with fresh namespace tokens, and requests
    // for new tokens are never suppressed as already acknowledged.
    if state.observations.output_topology_dirty || !state.observations.dirty_outputs.is_empty() {
        let live: BTreeSet<_> = state.backend.port_outputs().into_iter().map(|o| o.name).collect();
        state.observations.panel_holders.retain(|(output, _), _| live.contains(output));
    }
    // A mode RPC can arrive before its layer maps on the other connection,
    // and concealment destroys the layer; both are surface edges. The token
    // only finds the layer: it binds under the edge's incarnation fencing, so
    // a copy of it on another client's layer never becomes the panel, and an
    // unowned edge is adopted only for the token a registered holder service
    // reported.
    if !state.observations.pending_surface_edges.is_empty() {
        let resolved: Vec<_> = state.observations.panel_holders.iter()
            .map(|(key, panel)| {
                let id = resolve_panel_surface(panel_surface_candidates(state, &panel.surface, &key.0))
                    .ok()
                    .flatten();
                // Only for the very token the holder service itself named.
                let may_adopt = panel.reporter.as_deref().is_some_and(|reporter| !reporter.is_empty())
                    && panel.reported_surface.as_deref() == Some(panel.surface.as_str());
                let claim = id.map(|id| panel_claim(state, panel.owner.as_ref(), id, may_adopt));
                (key.clone(), id, claim)
            })
            .collect();
        for (key, id, claim) in resolved {
            if let Some(panel) = state.observations.panel_holders.get_mut(&key) {
                panel.id = match claim {
                    Some(claim) if claim.binds() => {
                        claim.apply(panel);
                        id
                    }
                    _ => None,
                };
            }
        }
    }
    track_panel_holders(state, Instant::now());
}

/// The surface is still a layer comp knows: a destroyed layer (or its role)
/// is a closed popup.
fn layer_alive(state: &WaylandState, id: SurfaceId) -> bool {
    state
        .surface_objects
        .get(&id)
        .and_then(|object| state.surfaces.get(object))
        .is_some_and(|record| matches!(record.role, super::SurfaceRole::Layer(_)))
}

/// A live layer's mapped state and its Wayland client; `None` once the
/// layer (or its role) is gone.
fn layer_facts(state: &WaylandState, id: SurfaceId) -> Option<(bool, Option<ClientId>)> {
    let record = state
        .surface_objects
        .get(&id)
        .and_then(|object| state.surfaces.get(object))
        .filter(|record| matches!(record.role, super::SurfaceRole::Layer(_)))?;
    Some((record.mapped, record.role.wl_surface().client().map(|client| client.id())))
}

/// What a resolved layer may do to an edge's incarnation.
#[derive(Clone, Debug, PartialEq)]
enum Claim {
    /// The layer is the owner's.
    Accept,
    /// The edge has no owner, and a registered holder service's mode report
    /// named this layer's token: its client becomes the owner.
    Adopt(ClientId),
    /// The owner is gone without its disconnect having been handled yet, and
    /// a registered holder service reported this layer: its client is the
    /// next incarnation, and nothing of the last one survives into it.
    Replace(ClientId),
    /// No owner, and nothing entitles this layer to adopt the edge (a hold,
    /// an anonymous report, or a token no registered service reported).
    Unowned,
    /// Another live client owns the edge: a copied token.
    Refuse,
}

impl Claim {
    fn binds(&self) -> bool {
        matches!(self, Self::Accept | Self::Adopt(_) | Self::Replace(_))
    }

    fn apply(self, panel: &mut PanelHolders) {
        match self {
            Self::Accept => {}
            Self::Adopt(client) => panel.owner = Some(client),
            Self::Replace(client) => {
                panel.drop_incarnation();
                panel.owner = Some(client);
            }
            Self::Unowned | Self::Refuse => {}
        }
    }
}

/// Incarnation fencing by Wayland client, which comp itself attests. A layer
/// whose client comp cannot name claims nothing. `may_adopt`: the token was
/// named by a mode report from a registered holder service. Tokens carry 128
/// random bits, so a client cannot guess one; any Bus peer that can read the
/// panel topics can still copy one and report it — the mesh is the trust
/// boundary, so that peer is trusted like any other.
fn panel_claim(
    state: &WaylandState,
    owner: Option<&ClientId>,
    id: SurfaceId,
    may_adopt: bool,
) -> Claim {
    let Some((_, Some(client))) = layer_facts(state, id) else {
        return Claim::Refuse;
    };
    match owner {
        Some(owner) if *owner == client => Claim::Accept,
        None if may_adopt => Claim::Adopt(client),
        None => Claim::Unowned,
        Some(owner) if state.display_handle.backend_handle().get_client_data(owner.clone()).is_ok() => {
            Claim::Refuse
        }
        Some(_) if may_adopt => Claim::Replace(client),
        Some(_) => Claim::Unowned,
    }
}

/// Publish the union of every edge's enforced layers to the visibility
/// funnel. Returns whether anything changed (visibility and focus moved).
fn apply_panel_enforcement(state: &mut WaylandState) -> bool {
    // A stopped owner keeps no keyboard grab: its Exclusive layers count as
    // on-demand, so applications get the keyboard back.
    let stalled: Vec<ClientId> = state
        .observations
        .panel_holders
        .values()
        .filter(|panel| panel.stalled)
        .filter_map(|panel| panel.owner.clone())
        .collect();
    let stalled_changed = stalled != state.observations.stalled_owners;
    if stalled_changed {
        tracing::info!(owners = stalled.len(), "stalled panel owners changed");
        state.observations.stalled_owners = stalled;
        state.arbitrate_keyboard_focus(None, false, false);
    }
    let enforced: BTreeSet<SurfaceId> = state
        .observations
        .panel_holders
        .values()
        .flat_map(|panel| panel.enforced.iter().copied())
        .collect();
    if enforced == state.observations.enforced_surfaces {
        return stalled_changed;
    }
    let added = enforced.difference(&state.observations.enforced_surfaces).count();
    let lifted = state.observations.enforced_surfaces.difference(&enforced).count();
    tracing::info!(added, lifted, total = enforced.len(), "panel enforcement changed");
    state.observations.enforced_surfaces = enforced;
    state.recompute_effective_visibility();
    state.retarget_pointer_after_visibility_change();
    true
}

/// Per-edge counts for the volatile `input.corners.enforced.*` and
/// `input.corners.held.*` leaves, summed over outputs.
pub(super) fn panel_edge_counts(state: &WaylandState) -> (EdgeCounts, EdgeCounts) {
    let mut enforced = EdgeCounts::default();
    let mut held = EdgeCounts::default();
    for ((_, edge), panel) in &state.observations.panel_holders {
        if let (Some(enforced), Some(held)) = (enforced.edge_mut(edge), held.edge_mut(edge)) {
            *enforced += panel.enforced.len() as u64;
            *held += panel.held.len() as u64;
        }
    }
    (enforced, held)
}

/// The owning Wayland client disconnected (Quoin crashed or exited): its
/// incarnation ends on every edge it owned. The next stable boundary settles
/// the verdicts, so a panel nothing else holds is concealed normally.
pub(super) fn panel_owner_disconnected(state: &mut WaylandState, client: &ClientId) {
    for panel in state.observations.panel_holders.values_mut() {
        if panel.owner.as_ref() == Some(client) {
            panel.drop_incarnation();
        }
    }
}

/// A registry diff (the full registered set): a holder service that left the
/// Bus takes its explicit holds with it. The Wayland-side state — owner,
/// enforcement, comp's own pointer and focus holders — stays with the Wayland
/// client, so a stalled Quoin that also lost its Bus connection is still
/// hidden. The next stable boundary settles the verdicts. This is this
/// node's registry: a reporter reaching comp over the mesh is not in it, so
/// its holds drop on the next local registry change (the liveness probe and
/// its next report repair that). noded caps each path's diffs at 10 Hz, so a
/// diff can be dropped; every diff carries the full set, so the next repairs
/// it.
pub(super) fn panel_services_live(state: &mut WaylandState, live: &BTreeSet<String>) {
    for panel in state.observations.panel_holders.values_mut() {
        if panel.reporter.as_ref().is_some_and(|reporter| !live.contains(reporter)) {
            panel.drop_holds();
            panel.reporter = None;
            panel.reported_surface = None;
            panel.generation = None;
        }
    }
}

/// Whether a layer belongs to an owner that left a probe unanswered (see
/// [`apply_panel_enforcement`]): arbitration treats its Exclusive
/// interactivity as on-demand.
pub(super) fn layer_owner_stalled(state: &WaylandState, client: Option<ClientId>) -> bool {
    client.is_some_and(|client| state.observations.stalled_owners.contains(&client))
}

fn focus_surface_id(
    state: &WaylandState,
    target: Option<super::focus::SeatFocusTarget>,
) -> Option<SurfaceId> {
    target
        .and_then(|target| target.surface_id())
        .and_then(|object| state.surfaces.get(&object))
        .map(|record| record.id)
}

/// Observe every reported panel's membership at `now`, emit the verdicts that
/// changed, run the stalled-owner checks (shell design §7) and arm the
/// earliest deadline. Returns whether enforcement moved visibility or focus.
///
/// The checks, all on one-shot deadlines:
/// - A conceal that ends a reveal comp commanded records the owner's layers
///   showing then ([`PanelHolders::pending`]). One unmapping is Quoin applying
///   it: nothing is owed.
/// - Past [`ENFORCE_GRACE`] with the conceal still unapplied — those layers,
///   or any owner layer shown while the verdict stays conceal (a stalled
///   intro or explicit show) — comp probes the owner: an unchanged configure
///   whose `ack_configure` a live client sends and a stopped one cannot.
/// - A button or key press outside the owner's surfaces while only a popup
///   or focus hold keeps the edge revealed (a stopped menu or launcher) is
///   probed at once.
/// - An answered probe owes nothing until the next trigger. One unanswered
///   for [`PROBE_TIMEOUT`] marks the owner stalled: its popup and focus holds
///   drop, its keyboard focus stops counting and its Exclusive grab goes, the
///   edge conceals, and the showing layers are hidden and excluded at once.
fn track_panel_holders(state: &mut WaylandState, now: Instant) -> bool {
    let pointer = focus_surface_id(state, state.pointer.current_focus());
    let keyboard = focus_surface_id(state, state.keyboard.current_focus());
    let user_input = std::mem::take(&mut state.observations.user_input);
    // The hotspot the pointer is in, by output key; dwelling engages it.
    let detector = &state.observations.corner_detector;
    let corner = state
        .observations
        .corner_output
        .and_then(|index| state.observations.corner_output_keys.get(index))
        .zip(detector.contact_corner())
        .map(|(key, corner)| (key.clone(), corner, detector.engaged_corner() == Some(corner)));
    // Every layer the holder state names, looked up once: `None` once gone.
    let layers: BTreeMap<SurfaceId, Option<(bool, Option<ClientId>)>> = state
        .observations
        .panel_holders
        .values()
        .flat_map(|panel| {
            panel
                .id
                .into_iter()
                .chain(panel.held.values().map(|(_, id)| *id))
                .chain(panel.popups.iter().copied())
                .chain(panel.pending.iter().copied())
                .chain(panel.enforced.iter().copied())
        })
        .map(|id| (id, layer_facts(state, id)))
        .collect();
    let alive = |id: &SurfaceId| layers.get(id).is_some_and(Option::is_some);
    let mapped = |id: &SurfaceId| {
        layers.get(id).is_some_and(|facts| facts.as_ref().is_some_and(|(mapped, _)| *mapped))
    };
    let mut commands = Vec::new();
    let mut probes = Vec::new();
    // At most one press-triggered probe per owner in flight; an owner that
    // just answered rests for PROBE_TIMEOUT.
    let mut probing: Vec<ClientId> = state
        .observations
        .panel_holders
        .values()
        .filter(|panel| panel.probe.is_some())
        .filter_map(|panel| panel.owner.clone())
        .collect();
    let mut deadline: Option<Instant> = None;
    for (key, panel) in &mut state.observations.panel_holders {
        let (output, edge) = key;
        // A popup's closing is its release, whether or not Quoin's release
        // request has arrived yet (it may be stalled, or overtaken by the
        // destruction).
        panel.held.retain(|kind, (_, id)| kind != "popup" || alive(&*id));
        panel.popups.retain(alive);
        // A layer its client has unmapped or destroyed is hidden by the
        // client itself: comp's enforcement of it is over.
        panel.enforced.retain(mapped);
        // A recorded layer unmapped: Quoin applied the conceal.
        if panel.pending.iter().any(|id| !mapped(id)) {
            panel.settle_owed();
        }
        // A probe went unanswered: the owner is stopped.
        if panel.probe.as_ref().is_some_and(|probe| probe.deadline <= now) {
            tracing::warn!(output = %output, edge = %edge, "panel owner left a liveness probe unanswered");
            panel.probe = None;
            panel.stall();
        }
        let on_hotspot = corner.as_ref().filter(|(key, corner, _)| {
            corner.summoned_edge() == edge.as_str() && *key == super::workspaces::output_key(output)
        });
        let over = |target: Option<SurfaceId>| {
            target.is_some_and(|target| {
                panel.id == Some(target) || panel.held.values().any(|(_, id)| *id == target)
            })
        };
        let seen = Membership {
            dwelled: on_hotspot.is_some_and(|(_, _, engaged)| *engaged),
            hotspot: on_hotspot.is_some(),
            surface: over(pointer),
            // A stopped owner's keyboard focus is about to be taken away.
            focused: !panel.stalled && over(keyboard),
        };
        panel.observe(seen, now);
        panel.expire(now);
        let previous = panel.verdict;
        if let Some(reveal) = panel.settle(false) {
            panel.note_verdict(reveal, previous);
            commands.push((output.clone(), edge.clone(), panel.surface.clone(), reveal));
        }
        // The owner's layers this edge shows: the panel's and its popups',
        // resolved identities only, never a token or prefix match.
        let owner = panel.owner.clone();
        let showing: BTreeSet<SurfaceId> = panel
            .id
            .into_iter()
            .chain(panel.popups.iter().copied())
            .filter(|id| {
                owner.is_some()
                    && layers.get(id).is_some_and(|facts| {
                        facts.as_ref().is_some_and(|(mapped, client)| *mapped && *client == owner)
                    })
            })
            .collect();
        let owes = panel.hidden() && panel.verdict == Some(false);
        if std::mem::take(&mut panel.arm_pending) && owes && !showing.is_empty() {
            panel.pending.clone_from(&showing);
            panel.enforce_at = Some(now + ENFORCE_GRACE);
        }
        if showing.is_empty() {
            panel.quiet = false;
        }
        if !owes {
            // Nothing to conceal; a probe of a revealed edge (a menu or
            // launcher the user clicked away from) stands.
            panel.pending.clear();
            panel.enforce_at = None;
        } else if panel.stalled && panel.enforced.is_empty() && !showing.is_empty() {
            // A stopped owner's conceal is enforced without another grace.
            panel.settle_owed();
            panel.enforced = showing.clone();
        } else if panel.pending.is_empty()
            && panel.enforce_at.is_none()
            && panel.probe.is_none()
            && panel.enforced.is_empty()
            && !panel.quiet
            && !showing.is_empty()
        {
            // Shown while comp says conceal (an intro or explicit show that
            // may belong to a stopped Quoin): the same grace, then a probe.
            panel.enforce_at = Some(now + ENFORCE_GRACE);
        }
        if owes && panel.enforce_at.is_some_and(|at| at <= now) {
            panel.enforce_at = None;
            let candidates: Vec<SurfaceId> = if panel.pending.is_empty() {
                showing.iter().copied().collect()
            } else {
                panel.pending.iter().copied().filter(|id| showing.contains(id)).collect()
            };
            if !candidates.is_empty() && panel.probe.is_none() {
                probes.push((key.clone(), candidates));
            }
        }
        // Only a popup or focus hold keeps the edge revealed, and the user
        // pressed somewhere the owner does not own: is the owner still there?
        let only_popup_or_focus = panel.verdict == Some(true)
            && panel.pointer == PointerHold::Out
            && !panel.held.contains_key("pointer")
            && (panel.focused || panel.held.contains_key("popup") || panel.held.contains_key("focus"));
        if only_popup_or_focus
            && panel.probe.is_none()
            && !panel.stalled
            && !showing.is_empty()
            && panel.probe_rest_until.is_none_or(|at| now >= at)
            && user_input.iter().any(|client| *client != owner)
            && let Some(client) = owner.clone()
            && !probing.contains(&client)
        {
            probing.push(client);
            probes.push((key.clone(), showing.iter().copied().collect()));
        }
        let probe_at = panel.probe.as_ref().map(|probe| probe.deadline);
        for at in [panel.conceal_deadline(), panel.enforce_at, probe_at].into_iter().flatten() {
            deadline = Some(deadline.map_or(at, |current| current.min(at)));
        }
    }
    for (key, candidates) in probes {
        let serials: Vec<(SurfaceId, smithay::utils::Serial)> = candidates
            .into_iter()
            .filter_map(|id| send_probe_configure(state, id).map(|serial| (id, serial)))
            .collect();
        if serials.is_empty() {
            continue;
        }
        let at = now + PROBE_TIMEOUT;
        if let Some(panel) = state.observations.panel_holders.get_mut(&key) {
            tracing::debug!(output = %key.0, edge = %key.1, layers = serials.len(), "probing panel owner");
            panel.probe = Some(Probe { deadline: at, serials });
            deadline = Some(deadline.map_or(at, |current| current.min(at)));
        }
    }
    for (output, edge, surface, reveal) in commands {
        state.observations.offer(|event_seq| ObservationRecord::PanelCommand {
            output, edge, surface, reveal, event_seq,
        });
    }
    rearm_conceal_timer(state, deadline);
    apply_panel_enforcement(state)
}

/// Re-send a layer's current configure, unchanged: a live client answers
/// with `ack_configure` (see [`note_layer_ack`]).
fn send_probe_configure(state: &WaylandState, id: SurfaceId) -> Option<smithay::utils::Serial> {
    let record = state.surface_objects.get(&id).and_then(|object| state.surfaces.get(object))?;
    let super::SurfaceRole::Layer(layer) = &record.role else {
        return None;
    };
    Some(layer.surface.layer_surface().send_configure())
}

/// A layer acknowledged a configure: its client is alive. An acknowledgement
/// at or after a probe's serial answers that probe (nothing is owed until the
/// next trigger), and any acknowledgement clears a stalled mark on its owner.
pub(super) fn note_layer_ack(
    state: &mut WaylandState,
    id: SurfaceId,
    client: Option<ClientId>,
    serial: smithay::utils::Serial,
) {
    let mut answered = false;
    for panel in state.observations.panel_holders.values_mut() {
        if panel.probe.as_ref().is_some_and(|probe| {
            probe.serials.iter().any(|(probed, sent)| *probed == id && serial >= *sent)
        }) {
            panel.settle_owed();
            panel.quiet = true;
            answered = true;
        }
        if client.is_some() && panel.owner == client {
            panel.stalled = false;
        }
    }
    // An owner that answered a probe rests: no press re-probes it for
    // PROBE_TIMEOUT, however often the user clicks elsewhere.
    if answered && client.is_some() {
        let rest = Instant::now() + PROBE_TIMEOUT;
        for panel in state.observations.panel_holders.values_mut() {
            if panel.owner == client {
                panel.probe_rest_until = Some(rest);
            }
        }
    }
}

/// A button or key press reached `client` (the surface pressed, or the
/// keyboard focus); see [`track_panel_holders`].
pub(super) fn note_user_input(state: &mut WaylandState, client: Option<ClientId>) {
    if !state.observations.panel_holders.is_empty() {
        state.observations.user_input.push(client);
    }
}

/// One-shot: the callback only clears the registration; the dispatch-cycle
/// epilogue that follows expires the pointer and emits the conceal.
fn rearm_conceal_timer(state: &mut WaylandState, deadline: Option<Instant>) {
    let observations = &mut state.observations;
    if deadline == observations.conceal_deadline
        && (deadline.is_none() || observations.conceal_timer.is_some())
    {
        return;
    }
    if let Some(token) = observations.conceal_timer.take() {
        observations.loop_handle.remove(token);
    }
    observations.conceal_deadline = None;
    let Some(deadline) = deadline else {
        return;
    };
    let delay = deadline.saturating_duration_since(Instant::now());
    let timer = observations
        .loop_handle
        .insert_source(Timer::from_duration(delay), |_, _, state| {
            state.observations.conceal_timer = None;
            state.observations.conceal_deadline = None;
            #[cfg(test)]
            {
                state.observations.conceal_timer_fired += 1;
            }
            TimeoutAction::Drop
        });
    match timer {
        Ok(token) => {
            observations.conceal_timer = Some(token);
            observations.conceal_deadline = Some(deadline);
            #[cfg(test)]
            {
                observations.conceal_timer_arms += 1;
            }
        }
        Err(error) => tracing::warn!(%error, "panel conceal timer unavailable"),
    }
}

/// Popup bookkeeping for one reported keyboard focus change. A held popup
/// taking focus records what it displaced; focus leaving a popup that still
/// lives is a deliberate move and cancels its restoration; focus leaving a
/// destroyed popup records where comp's fallback put it.
fn note_popup_focus(state: &mut WaylandState, to: Option<u64>, from: Option<u64>) {
    if state.observations.popup_restores.is_empty() {
        return;
    }
    let from_alive = from.is_some_and(|id| layer_alive(state, SurfaceId(id)));
    let restores = &mut state.observations.popup_restores;
    let to_held_popup = to.is_some_and(|to| restores.contains_key(&to));
    if let Some(to) = to
        && let Some(entry) = restores.get_mut(&to)
    {
        // Focus came back: whatever departure was recorded is undone.
        entry.departed_to = None;
        if entry.prior.is_none() {
            entry.prior = from;
        }
    }
    if let Some(from) = from
        && let Some(entry) = restores.get_mut(&from)
    {
        if !from_alive {
            entry.fallback = Some(to);
        } else if !to_held_popup {
            // Deliberate, unless `to` is a popup whose hold is still to come
            // (an exclusive menu takes focus as it maps): see record_popup_focus.
            entry.departed_to = Some(to);
        }
    }
}

/// A closed popup hands keyboard focus back to what it displaced (a toplevel
/// or a layer such as the panel itself) — only when its destruction is what
/// moved focus and focus is still where comp's fallback put it (the top
/// toplevel, or nothing). Runs after the focus edge, so the destruction's own
/// focus change is already recorded; returns whether focus moved.
fn service_popup_restores(state: &mut WaylandState) -> bool {
    if state.observations.popup_restores.is_empty() {
        return false;
    }
    let closed: Vec<(u64, PopupRestore)> = state
        .observations
        .popup_restores
        .iter()
        .filter(|(popup, _)| !layer_alive(state, SurfaceId(**popup)))
        .map(|(popup, restore)| (*popup, *restore))
        .collect();
    let mut restored = false;
    for (popup, restore) in closed {
        state.observations.popup_restores.remove(&popup);
        let (Some(prior), Some(fallback), None) =
            (restore.prior, restore.fallback, restore.departed_to)
        else {
            continue;
        };
        let current = focus_surface_id(state, state.keyboard.current_focus()).map(|id| id.0);
        if current != fallback || current == Some(prior) {
            continue;
        }
        let Some(surface) = state
            .surface_objects
            .get(&SurfaceId(prior))
            .and_then(|object| state.surfaces.get(object))
            .filter(|record| record.mapped)
            .map(|record| record.role.wl_surface().clone())
        else {
            continue;
        };
        state.arbitrate_keyboard_focus(Some(surface), false, false);
        restored = true;
    }
    restored
}

/// Resolve the exact namespace on the named output. Wayland object numbers
/// alone are client-local and are deliberately not accepted as identities.
fn service_panel_request(state: &mut WaylandState, request: &PanelRequest) -> ControlReply {
    if state.session_lock_active() { return ControlReply::Locked; }
    if !state.backend.port_outputs().iter().any(|output| output.name == request.output) {
        return ControlReply::refused("unknown_output", json!({"output":request.output}));
    }
    let id = match resolve_panel_surface(panel_surface_candidates(state, &request.surface, &request.output)) {
        Ok(id) => id,
        Err(code) => return ControlReply::refused(code, json!({"surface":request.surface})),
    };
    // A concealed panel has no layer. Its mode still exists. Release also
    // remains valid after the popup's Wayland destruction overtakes its RPC.
    if id.is_none() && request.acquire == Some(true) {
        return ControlReply::refused("unknown_panel_surface", json!({"surface":request.surface}));
    }
    let key = (request.output.clone(), request.edge.clone());
    let (owner, reporter, current_generation) = state
        .observations
        .panel_holders
        .get(&key)
        .map_or((None, None, None), |panel| (panel.owner.clone(), panel.reporter.clone(), panel.generation));
    // The holder service is the first registered (broker-stamped) service to
    // report the edge; only it reports it after that. A report from anyone
    // else is recorded but binds nothing and never replaces the token the
    // holder named.
    let from_reporter = !request.sender.is_empty()
        && reporter.as_deref().is_none_or(|reporter| reporter == request.sender);
    let reporting = request.mode.is_some() && from_reporter;
    let foreign_report = request.mode.is_some() && !reporting && reporter.is_some();
    // Generations only move forward: a report from an older Bus connection
    // of the holder (delayed, or from a dead incarnation) changes nothing.
    if reporting
        && let (Some(generation), Some(current)) = (request.generation, current_generation)
        && generation < current
    {
        tracing::warn!(generation, current, edge = %request.edge, "stale holder generation refused");
        return ControlReply::refused("stale_generation", json!({"generation":generation,"current":current}));
    }
    // Incarnation fencing: a layer names the edge's owner by its Wayland
    // client, which comp attests; a copied token on another live client's
    // layer is refused rather than bound, so it can never be held, tracked or
    // enforced. Only the registered holder service adopts an unowned edge:
    // by a mode report, or by a hold once it has reported the edge (a corner
    // menu can open while its panel has no layer).
    let may_adopt = reporting || (from_reporter && reporter.is_some());
    let claim = id.map(|id| panel_claim(state, owner.as_ref(), id, may_adopt));
    match &claim {
        Some(Claim::Refuse) => {
            return ControlReply::refused("panel_owner_mismatch", json!({"surface":request.surface}));
        }
        // No owned panel is there to hold yet: the layer's edge binds when
        // its registered holder's mode report does (a mapping retries this).
        Some(Claim::Unowned) if request.acquire == Some(true) => {
            return ControlReply::refused("unknown_panel_surface", json!({"surface":request.surface}));
        }
        _ => {}
    }
    let bound = !foreign_report && claim.as_ref().is_some_and(Claim::binds);
    let id = id.filter(|_| bound);
    let recorded = state
        .observations
        .panel_holders
        .get(&key)
        .map(|panel| (panel.surface.clone(), panel.id));
    if let Some(panel) = state.observations.panel_holders.get_mut(&key) {
        // A replacement drops the dead incarnation before this request lands.
        if let Some(replace @ Claim::Replace(_)) = claim.clone()
            && bound
        {
            replace.apply(panel);
        }
        // A newer Bus generation of the holder is a new Bus incarnation: its
        // predecessor's holds end here.
        if reporting
            && let (Some(generation), Some(current)) = (request.generation, panel.generation)
            && generation > current
        {
            panel.drop_holds();
        }
    }
    if request.holder.as_deref() == Some("popup")
        && request.acquire == Some(true)
        && let Some(popup) = id
    {
        record_popup_focus(state, popup);
    }
    // Resynchronisation: a report lifts comp's exclusion; if a conceal was
    // still owed, it is owed again with a fresh grace (below).
    let owed = request.mode.is_some()
        && state.observations.panel_holders.get(&key).is_some_and(PanelHolders::owed);
    let previous = state.observations.panel_holders.get(&key).and_then(|panel| panel.verdict);
    let verdict = apply_panel_request(&mut state.observations.panel_holders, request, id);
    if let Some(panel) = state.observations.panel_holders.get_mut(&key) {
        if foreign_report && let Some((surface, id)) = recorded {
            panel.surface = surface;
            panel.id = id;
        }
        if let Some(adopt @ Claim::Adopt(_)) = claim
            && bound
        {
            adopt.apply(panel);
        }
        if reporting {
            panel.reporter = Some(request.sender.clone());
            panel.reported_surface = Some(request.surface.clone());
            panel.generation = request.generation.or(panel.generation);
        }
        // The holder answered on the Bus: it is not stopped.
        if bound || reporting {
            panel.stalled = false;
        }
        if request.holder.as_deref() == Some("popup")
            && request.acquire == Some(true)
            && let Some(popup) = id
        {
            panel.popups.insert(popup);
        }
        if let Some(reveal) = verdict {
            panel.note_verdict(reveal, previous);
        }
        if owed && panel.hidden() && panel.verdict == Some(false) {
            panel.arm_pending = true;
        }
    }
    if let Some(reveal) = verdict {
        // Commands name the panel's own token when comp has one; Quoin
        // accepts either its current panel or popup token for the edge.
        let surface = state.observations.panel_holders.get(&key)
            .map_or_else(|| request.surface.clone(), |panel| panel.surface.clone());
        state.observations.offer(|event_seq| ObservationRecord::PanelCommand {
            output: request.output.clone(), edge: request.edge.clone(),
            surface, reveal, event_seq,
        });
    }
    ControlReply::Body(json!({"accepted":true,"surface":request.surface}))
}

/// Start tracking the focus a popup displaces. An exclusive layer usually
/// takes focus as it maps, before its hold arrives: the displaced focus is
/// then the one its own focus change replaced. A popup without focus yet
/// records what it displaces when it takes focus ([`note_popup_focus`]).
fn record_popup_focus(state: &mut WaylandState, popup: SurfaceId) {
    let current = focus_surface_id(state, state.keyboard.current_focus()).map(|id| id.0);
    let prior = (current == Some(popup.0))
        .then(|| {
            state
                .observations
                .last_focus_change
                .filter(|(to, _)| *to == Some(popup.0))
                .and_then(|(_, from)| from)
        })
        .flatten()
        .filter(|prior| *prior != popup.0);
    // A popup that focus left for this one was not left deliberately: it
    // is the parent of a nested menu, restored when this one closes.
    for entry in state.observations.popup_restores.values_mut() {
        if entry.departed_to == Some(Some(popup.0)) {
            entry.departed_to = None;
        }
    }
    state
        .observations
        .popup_restores
        .entry(popup.0)
        .or_insert(PopupRestore { prior, fallback: None, departed_to: None });
}

fn service_set(
    state: &mut WaylandState,
    request: &mut PortSetRequest,
    changes: &mut PendingPropChanges,
) {
    let path = request.path.clone();
    #[cfg(feature = "xwayland")]
    if path == "xwayland.enabled" {
        service_set_xwayland_enabled(state, request, changes);
        return;
    }
    if let Some((window, leaf)) = parse_window_leaf_path(&path) {
        // The optional fence is checked before any leaf logic, so a stale
        // write never reaches whatever window inherited the id.
        if let Some(generation) = request.generation
            && let Err(error @ WindowTargetError::StaleTarget { .. }) =
                state.resolve_window_target(window, Some(generation))
        {
            if let Some(reply) = request.reply.take() {
                let _ = reply.send(ControlReply::WindowTarget { id: window, error });
            }
            return;
        }
        match leaf {
            "band" => service_set_window_band(state, request, window),
            "minimized" => service_set_window_minimized(state, request, window),
            "maximized" | "fullscreen" => service_set_window_state(state, request, window, leaf),
            "workspace" => service_set_window_workspace(state, request, window),
            _ => {
                if let Some(reply) = request.reply.take() {
                    let _ = reply.send(ControlReply::Validation(read_only_or_unknown(&path)));
                }
            }
        }
        return;
    }
    if request.generation.is_some() {
        if let Some(reply) = request.reply.take() {
            let _ = reply.send(ControlReply::Validation(invalid_value(
                "generation",
                "absent",
                "generation applies to windows.s<id>.* paths only",
            )));
        }
        return;
    }
    if let Some(target) = parse_workspaces_set_path(&path) {
        service_set_workspaces(state, request, target);
        return;
    }
    if path == HOST_PASSTHROUGH_PATH {
        service_set_host_passthrough(state, request, changes);
        return;
    }
    let old_config = state.observations.corner_config;
    let mut new_config = old_config;
    let validated = match validate_corner_value(&path, &request.value) {
        Ok(value) => value,
        Err(error) => {
            if let Some(reply) = request.reply.take() {
                let _ = reply.send(ControlReply::Validation(error));
            }
            return;
        }
    };
    let (old, new) = apply_corner_value(&mut new_config, validated);
    if old != new {
        state.apply_corner_config(new_config);
        queue_prop_change(changes, path.clone(), old.clone(), new.clone(), "props.set");
    }
    if let Some(reply) = request.reply.take() {
        let _ = reply.send(ControlReply::Set {
            path,
            old,
            new,
            persisted: None,
        });
    }
}

pub(crate) const HOST_PASSTHROUGH_PATH: &str = "input.host.passthrough";

/// `input.host.passthrough` (nested only): `false` stops host pointer and
/// key input reaching the seat, so the host cursor cannot overwrite an
/// injected position. Process-lifetime; the leaf does not exist on kms.
fn service_set_host_passthrough(
    state: &mut WaylandState,
    request: &mut PortSetRequest,
    changes: &mut PendingPropChanges,
) {
    let path = request.path.clone();
    let reply = if !state.host_passthrough_available() {
        ControlReply::Validation(SetValidationError::UnknownPath)
    } else if let Some(value) = request.value.as_bool() {
        let old = state.host_passthrough();
        if old != value {
            state.set_host_passthrough(value);
            queue_prop_change(
                changes,
                path.clone(),
                PropValue::Bool(old),
                PropValue::Bool(value),
                "props.set",
            );
        }
        ControlReply::Set {
            path,
            old: PropValue::Bool(old),
            new: PropValue::Bool(value),
            persisted: None,
        }
    } else {
        ControlReply::Validation(invalid_value(&path, "bool", "true|false"))
    };
    if let Some(sender) = request.reply.take() {
        let _ = sender.send(reply);
    }
}

/// The `xwayland.enabled` set: validate a bool, update the CONFIGURED
/// value (get/describe read it; the running lifecycle is untouched — the
/// switch is startup-read, see `XwaylandRuntime::enabled`), persist for
/// the next startup, and emit the changed event through the same queue as
/// every other leaf.
#[cfg(feature = "xwayland")]
fn service_set_xwayland_enabled(
    state: &mut WaylandState,
    request: &mut PortSetRequest,
    changes: &mut PendingPropChanges,
) {
    let path = request.path.clone();
    let Some(value) = request.value.as_bool() else {
        if let Some(reply) = request.reply.take() {
            let _ = reply.send(ControlReply::Validation(invalid_value(
                &path,
                "bool",
                "true|false",
            )));
        }
        return;
    };
    let old = state.xwayland.enabled;
    // TWO dedups with different subjects, deliberately separated: the
    // changed EVENT dedups on `old != value` (a no-op set publishes
    // nothing), but the PERSIST runs on every admitted set — the file is
    // the durability contract, the write is idempotent, and deduping it
    // would make the reply lie on exactly the paths an operator takes: the
    // retry after a persisted:false (old == value now, nothing written,
    // "persisted:true"), and making an env-override value durable (set
    // equals the in-memory value, nothing written, remove the env var and
    // it is gone). The write's outcome is REPORTED, not swallowed; the
    // in-memory change and the changed event stand on failure (no
    // rollback — refusing the set on an I/O error would leave no
    // structured channel at all; the env override remains the last
    // resort). The offline suite never writes the dev machine's real etc
    // tree — that arm reports true, and the real write/read round trip is
    // unit-tested against a tempdir.
    #[cfg(not(test))]
    let persisted = {
        let target = super::xwayland::xwayland_enabled_persist_path(&state.xwayland.socket_name);
        super::xwayland::write_xwayland_enabled(&target, value).is_ok()
    };
    #[cfg(test)]
    let persisted = {
        // The real write is compiled out under test (the offline suite
        // never touches the machine's etc tree; the tempdir round trip
        // covers it) — the counter is what keeps the every-admitted-set
        // placement red-capable.
        state.x11_persist_attempts += 1;
        true
    };
    if old != value {
        state.xwayland.enabled = value;
        queue_prop_change(
            changes,
            path.clone(),
            PropValue::Bool(old),
            PropValue::Bool(value),
            "props.set",
        );
    }
    if let Some(reply) = request.reply.take() {
        let _ = reply.send(ControlReply::Set {
            path,
            old: PropValue::Bool(old),
            new: PropValue::Bool(value),
            persisted: Some(persisted),
        });
    }
}

/// The `windows.s<id>.band` set: restack the window's whole role tree into
/// the requested band. No explicit prop-change queueing — the restack marks
/// the surface and stack dirty, and the observation flush diffs the window
/// row against the watched baseline, so `windows.s<id>.band` and `stack`
/// changed events flow through the same lane as every other window mutation.
/// Runtime state only: a band assignment is never persisted.
fn service_set_window_band(state: &mut WaylandState, request: &mut PortSetRequest, window: u64) {
    let path = request.path.clone();
    let band = match validate_window_band_value(&path, &request.value) {
        Ok(band) => band,
        Err(error) => {
            if let Some(reply) = request.reply.take() {
                let _ = reply.send(ControlReply::Validation(error));
            }
            return;
        }
    };
    let outcome = state.set_window_band(SurfaceId(window), band, "props.set");
    let reply_value = match outcome {
        Some((old, new)) => ControlReply::Set {
            path,
            old: PropValue::String(old.into()),
            new: PropValue::String(new.into()),
            persisted: None,
        },
        None => ControlReply::Validation(invalid_value(
            &path,
            "existing window id",
            "a live toplevel window",
        )),
    };
    if let Some(reply) = request.reply.take() {
        let _ = reply.send(reply_value);
    }
}

/// The `windows.s<id>.minimized` set: `true` minimises exactly like the
/// title-bar button, `false` restores THIS window (not the LIFO top) and
/// focuses and raises it. Like the band leaf, the changed events come from
/// the window-row diff, here attributed to `props.set`.
fn service_set_window_minimized(
    state: &mut WaylandState,
    request: &mut PortSetRequest,
    window: u64,
) {
    let path = request.path.clone();
    let Some(minimized) = request.value.as_bool() else {
        if let Some(reply) = request.reply.take() {
            let _ = reply.send(ControlReply::Validation(invalid_value(
                &path,
                "bool",
                "true|false",
            )));
        }
        return;
    };
    let reply_value = if state.session_lock_active() {
        ControlReply::Locked
    } else {
        match state.resolve_window_target(window, request.generation) {
            Ok(object) => {
                state.mark_surface_dirty(SurfaceId(window), "props.set");
                match state.set_window_minimized(&object, minimized) {
                    Some((old, new)) => ControlReply::Set {
                        path,
                        old: PropValue::Bool(old),
                        new: PropValue::Bool(new),
                        persisted: None,
                    },
                    None => missing_window(&path),
                }
            }
            Err(error @ WindowTargetError::StaleTarget { .. }) => {
                ControlReply::WindowTarget { id: window, error }
            }
            Err(_) => missing_window(&path),
        }
    };
    if let Some(reply) = request.reply.take() {
        let _ = reply.send(reply_value);
    }
}

fn service_set_window_state(
    state: &mut WaylandState,
    request: &mut PortSetRequest,
    window: u64,
    leaf: &str,
) {
    crate::frame_trace::event("comp_window_control", || {
        (window, 11, request.generation.unwrap_or(0))
    });
    let result = if let Some(enabled) = request.value.as_bool() {
        if state.session_lock_active() {
            ControlReply::Locked
        } else {
            match state.resolve_window_target(window, request.generation) {
                Ok(object) => {
                    let kind = if leaf == "maximized" {
                        crate::port::WindowState::Maximized
                    } else {
                        crate::port::WindowState::Fullscreen
                    };
                    match state.set_window_state(&object, kind, enabled, None, "props.set") {
                        Ok((old, new)) => ControlReply::Set {
                            path: request.path.clone(),
                            old: PropValue::Bool(old),
                            new: PropValue::Bool(new),
                            persisted: None,
                        },
                        Err(reply) => reply,
                    }
                }
                Err(error @ WindowTargetError::StaleTarget { .. }) => ControlReply::WindowTarget { id: window, error },
                Err(_) => missing_window(&request.path),
            }
        }
    } else {
        ControlReply::Validation(invalid_value(&request.path, "bool", "true|false"))
    };
    if let Some(reply) = request.reply.take() {
        let _ = reply.send(result);
    }
}

fn missing_window(path: &str) -> ControlReply {
    ControlReply::Validation(invalid_value(
        path,
        "existing window id",
        "a live toplevel window",
    ))
}

/// The `range` a workspace index refusal reports; the live bound is the
/// count, which the core checks.
const WORKSPACE_INDEX_RANGE: &str = "1..=count";
/// The `range` a count refusal reports — the same bound as the core's
/// `WORKSPACE_COUNT_MAX`, pinned so the two cannot drift.
const WORKSPACE_COUNT_RANGE: &str = "1..=16";
/// The `range` an output-key refusal reports: only the default output's
/// current workspace is switchable in 0.59 (D3), and its key is the one
/// `workspaces.current` addresses without naming it.
const WORKSPACE_OUTPUT_RANGE: &str =
    "the default output's o_<slug> (the only switchable output; workspaces.current addresses it)";
const _: () = assert!(
    WORKSPACE_COUNT_MAX == 16,
    "WORKSPACE_COUNT_RANGE names the core's cap"
);

/// Where a `workspaces.*` write is aimed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum WorkspacesSetTarget {
    Count,
    /// `workspaces.current` (`None`: the default output) or
    /// `workspaces.o_<slug>.current` (`Some(key)`; the core refuses a key
    /// that is not the default output's in 0.59, D3).
    Current(Option<String>),
}

/// Parse the three writable `workspaces.*` shapes. Everything else under
/// the subtree is read-only (`workspaces`, `workspaces.list`) or unknown.
pub(crate) fn parse_workspaces_set_path(path: &str) -> Option<WorkspacesSetTarget> {
    match path {
        "workspaces.count" => Some(WorkspacesSetTarget::Count),
        "workspaces.current" => Some(WorkspacesSetTarget::Current(None)),
        _ => {
            let key = path.strip_prefix("workspaces.")?.strip_suffix(".current")?;
            (key.starts_with("o_") && key.len() > 2 && !key.contains('.'))
                .then(|| WorkspacesSetTarget::Current(Some(key.to_string())))
        }
    }
}

/// A workspace index or count on the wire: an unsigned integer >= 1 that
/// fits a `u32`. The live upper bound (`count`, or the cap) is the core's
/// check, reported through the same `range`.
fn workspace_value(
    path: &str,
    value: &Value,
    range: &'static str,
) -> Result<u32, SetValidationError> {
    value
        .as_u64()
        .filter(|value| *value >= 1)
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| invalid_value(path, "integer", range))
}

/// The dirty marks a workspace write plants BEFORE the core runs, so the
/// diff the core schedules reads cause `props.set` (`full_dirty` and the
/// per-surface entry both keep their FIRST cause). A write the core then
/// refuses, or that changes nothing, must take its marks back out: a
/// planted `full_dirty` would otherwise attribute the next unrelated
/// change (another client's map, say) to a write that never took effect.
/// `restore` puts back exactly what was there — a cause planted earlier
/// by someone else was never overwritten, so it survives either way.
/// One surface mark planted ahead of an operation that may still refuse
/// (the verbs mark first so their cause wins over the core's), with enough
/// remembered to take it back: a mark that was already there stays.
pub(super) struct PlantedSurfaceMark {
    id: u64,
    had_entry: bool,
}

impl WaylandState {
    pub(super) fn plant_surface_mark(
        &mut self,
        id: u64,
        cause: &'static str,
    ) -> PlantedSurfaceMark {
        let had_entry = self.observations.dirty_surfaces.contains_key(&id);
        self.mark_surface_dirty(SurfaceId(id), cause);
        PlantedSurfaceMark { id, had_entry }
    }

    /// The operation changed nothing: the mark goes unless it predates it.
    pub(super) fn unplant_surface_mark(&mut self, mark: PlantedSurfaceMark) {
        if !mark.had_entry {
            self.observations.dirty_surfaces.remove(&mark.id);
        }
    }
}

struct PlantedWorkspaceMarks {
    full: Option<&'static str>,
    surface: Option<PlantedSurfaceMark>,
}

impl PlantedWorkspaceMarks {
    fn plant(state: &mut WaylandState, surface: Option<u64>) -> Self {
        let full = state.observations.full_dirty;
        let surface = surface.map(|id| state.plant_surface_mark(id, "props.set"));
        state.mark_workspaces_dirty("props.set");
        Self { full, surface }
    }

    /// The write changed nothing: back to the marks as they were.
    fn restore(self, state: &mut WaylandState) {
        state.observations.full_dirty = self.full;
        if let Some(mark) = self.surface {
            state.unplant_surface_mark(mark);
        }
    }

    /// Keep the marks only for a write that changed something.
    fn settle(self, state: &mut WaylandState, outcome: &Result<(u32, u32), WorkspaceRefusal>) {
        if !matches!(outcome, Ok((old, new)) if old != new) {
            self.restore(state);
        }
    }
}

/// The `windows.s<id>.workspace` set: move THIS window to the workspace
/// without switching (rule 5). The changed events come from the full
/// snapshot diff the core's dirty mark schedules, attributed to
/// `props.set` because the cause is planted before the core runs and
/// taken back out when the core refuses (`PlantedWorkspaceMarks`). A move
/// never bumps the generation, so a fenced retry after the move still
/// resolves.
fn service_set_window_workspace(
    state: &mut WaylandState,
    request: &mut PortSetRequest,
    window: u64,
) {
    let path = request.path.clone();
    let reply_value = match workspace_value(&path, &request.value, WORKSPACE_INDEX_RANGE) {
        Err(error) => ControlReply::Validation(error),
        Ok(_) if state.session_lock_active() => ControlReply::Locked,
        Ok(index) => match state.resolve_window_target(window, request.generation) {
            Ok(object) => {
                let marks = PlantedWorkspaceMarks::plant(state, Some(window));
                let outcome =
                    state.move_window_to_workspace(&object, WorkspaceTarget::Index(index));
                marks.settle(state, &outcome);
                match outcome {
                    Ok((old, new)) => ControlReply::Set {
                        path,
                        old: PropValue::U32(old),
                        new: PropValue::U32(new),
                        persisted: None,
                    },
                    Err(WorkspaceRefusal::NotAWindow) => missing_window(&path),
                    Err(_) => ControlReply::Validation(invalid_value(
                        &path,
                        "integer",
                        WORKSPACE_INDEX_RANGE,
                    )),
                }
            }
            Err(error @ WindowTargetError::StaleTarget { .. }) => {
                ControlReply::WindowTarget { id: window, error }
            }
            Err(_) => missing_window(&path),
        },
    };
    if let Some(reply) = request.reply.take() {
        let _ = reply.send(reply_value);
    }
}

/// The `workspaces.count` / `workspaces.current` /
/// `workspaces.o_<slug>.current` sets, each a direct call into the core
/// (`set_workspace_count`, `switch_workspace`). `Locked` under a session
/// lock like the window verbs (D12). No explicit change queueing: the
/// core marks the full snapshot dirty and the next observation diffs
/// `workspaces.*` and every window row (D7); the cause is planted first so
/// it reads `props.set`, and unplanted again on a refusal or a no-op
/// (`PlantedWorkspaceMarks`).
fn service_set_workspaces(
    state: &mut WaylandState,
    request: &mut PortSetRequest,
    target: WorkspacesSetTarget,
) {
    let path = request.path.clone();
    let range = match target {
        WorkspacesSetTarget::Count => WORKSPACE_COUNT_RANGE,
        WorkspacesSetTarget::Current(_) => WORKSPACE_INDEX_RANGE,
    };
    let reply_value =
        match workspace_value(&path, &request.value, range) {
            Err(error) => ControlReply::Validation(error),
            Ok(_) if state.session_lock_active() => ControlReply::Locked,
            Ok(value) => {
                let marks = PlantedWorkspaceMarks::plant(state, None);
                let outcome = match &target {
                    WorkspacesSetTarget::Count => state.set_workspace_count(value),
                    WorkspacesSetTarget::Current(key) => state
                        .switch_workspace(key.as_deref(), WorkspaceTarget::Index(value), true)
                        .map(|switch| (switch.from, switch.to)),
                };
                marks.settle(state, &outcome);
                match outcome {
                    Ok((old, new)) => ControlReply::Set {
                        path,
                        old: PropValue::U32(old),
                        new: PropValue::U32(new),
                        persisted: None,
                    },
                    // The key may well exist under `outputs.*`; what the
                    // core refuses is a key that is not the DEFAULT
                    // output's (D3), so the range says that, not "an
                    // existing key" the caller just read.
                    Err(WorkspaceRefusal::UnknownOutput) => ControlReply::Validation(
                        invalid_value(&path, "output key", WORKSPACE_OUTPUT_RANGE),
                    ),
                    Err(_) => ControlReply::Validation(invalid_value(&path, "integer", range)),
                }
            }
        };
    if let Some(reply) = request.reply.take() {
        let _ = reply.send(reply_value);
    }
}

fn flush_set_changes(state: &mut WaylandState, changes: PendingPropChanges) {
    flush_prop_changes(state, changes);
    let host = state.host_input_snapshot();
    if let Some(baseline) = state.observations.watched_baseline.as_mut() {
        baseline.input.corners = state.observations.corner_config.into();
        baseline.input.host = host;
        #[cfg(feature = "xwayland")]
        {
            baseline.xwayland.enabled = state.xwayland.enabled;
        }
    }
}

/// Parse a `windows.s<id>.band` write path into the window's surface id.
/// Only this exact shape is writable; every other `windows.*` path stays
/// read-only through `known_read_only_path`.
pub(crate) fn parse_window_band_path(path: &str) -> Option<u64> {
    parse_window_leaf_path(path).and_then(|(id, leaf)| (leaf == "band").then_some(id))
}

/// Parse `windows.s<id>.<leaf>` into the canonical id and the one-segment
/// leaf name. Used by the write gate and the `generation` fence.
pub(crate) fn parse_window_leaf_path(path: &str) -> Option<(u64, &str)> {
    let (id, leaf) = path.strip_prefix("windows.s")?.split_once('.')?;
    if leaf.is_empty() || leaf.contains('.') {
        return None;
    }
    // Canonical ids only: a leading zero ("windows.s0007.band") would write
    // through an alias that reads, describes and event-diffs as "s7" — the
    // reply would name a path that can never be read back.
    if id.is_empty()
        || (id.len() > 1 && id.starts_with('0'))
        || !id.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    Some((id.parse().ok()?, leaf))
}

/// Unknown and read-only window leaves keep the ordinary set errors.
fn read_only_or_unknown(path: &str) -> SetValidationError {
    if known_read_only_path(path) {
        SetValidationError::ReadOnly
    } else {
        SetValidationError::UnknownPath
    }
}

/// A window band write accepts exactly the two operator-reachable bands:
/// `bottom` (behind every normal window; Quoin corners stay live because
/// they are compositor-side hotspots) and `normal`. The other bands belong
/// to layer-shell and session-lock surfaces, never to toplevels.
pub(crate) fn validate_window_band_value(
    path: &str,
    value: &Value,
) -> Result<StackBand, SetValidationError> {
    match value.as_str() {
        Some("bottom") => Ok(StackBand::Bottom),
        Some("normal") => Ok(StackBand::Normal),
        _ => Err(invalid_value(path, "string", "bottom|normal")),
    }
}

/// The one ingress-side gate for `comp.props.set`: admits every writable
/// leaf family (corner config, the window band leaf, and — with the
/// feature — `xwayland.enabled`, whose service arm previously existed but
/// was unreachable because this gate only knew corner paths). Value
/// validation is repeated by the service arms; state-dependent checks
/// (does the window exist) belong to the service, not this gate.
pub(crate) fn validate_set_request(path: &str, value: &Value) -> Result<(), SetValidationError> {
    #[cfg(feature = "xwayland")]
    if path == "xwayland.enabled" {
        return if value.is_boolean() {
            Ok(())
        } else {
            Err(invalid_value(path, "bool", "true|false"))
        };
    }
    if parse_window_band_path(path).is_some() {
        return validate_window_band_value(path, value).map(|_| ());
    }
    if path == HOST_PASSTHROUGH_PATH {
        // Backend presence is the service's call (the leaf exists only on
        // the nested backend).
        return if value.is_boolean() {
            Ok(())
        } else {
            Err(invalid_value(path, "bool", "true|false"))
        };
    }
    if parse_window_leaf_path(path).is_some_and(|(_, leaf)| matches!(leaf, "minimized" | "maximized" | "fullscreen")) {
        return if value.is_boolean() {
            Ok(())
        } else {
            Err(invalid_value(path, "bool", "true|false"))
        };
    }
    // Workspace leaves: an integer >= 1 passes the gate; the live upper
    // bound (the count, or the cap) is the service's, reported through
    // the same range strings.
    if parse_window_leaf_path(path).is_some_and(|(_, leaf)| leaf == "workspace") {
        return workspace_value(path, value, WORKSPACE_INDEX_RANGE).map(|_| ());
    }
    match parse_workspaces_set_path(path) {
        Some(WorkspacesSetTarget::Count) => {
            return workspace_value(path, value, WORKSPACE_COUNT_RANGE).map(|_| ());
        }
        Some(WorkspacesSetTarget::Current(_)) => {
            return workspace_value(path, value, WORKSPACE_INDEX_RANGE).map(|_| ());
        }
        None => {}
    }
    validate_corner_value(path, value).map(|_| ())
}

pub(crate) fn validate_corner_value(
    path: &str,
    value: &Value,
) -> Result<ValidatedCornerValue, SetValidationError> {
    match path {
        "input.corners.holders" => Err(SetValidationError::ReadOnly),
        _ if ["input.corners.enforced", "input.corners.held"].iter().any(|counts| {
            path == *counts
                || path.strip_prefix(*counts).and_then(|rest| rest.strip_prefix('.')).is_some_and(
                    |edge| matches!(edge, "top" | "bottom" | "left" | "right"),
                )
        }) =>
        {
            Err(SetValidationError::ReadOnly)
        }
        "input.corners.enabled" => {
            let Some(value) = value.as_bool() else {
                return Err(invalid_value(path, "bool", "true|false"));
            };
            Ok(ValidatedCornerValue::Enabled(value))
        }
        "input.corners.deadzone_px" => {
            let value = finite_number(path, value, "finite number", "1.0..=256.0")?;
            if !(1.0..=256.0).contains(&value) {
                return Err(invalid_value(path, "finite number", "1.0..=256.0"));
            }
            Ok(ValidatedCornerValue::DeadzonePx(value))
        }
        "input.corners.dwell_ms" => {
            let Some(value) = value.as_u64().filter(|value| *value <= 5_000) else {
                return Err(invalid_value(path, "integer", "0..=5000"));
            };
            Ok(ValidatedCornerValue::DwellMs(value))
        }
        "input.corners.velocity_max_px_s" => {
            let value = finite_number(path, value, "finite number", "1.0..=20000.0")?;
            if !(1.0..=20_000.0).contains(&value) {
                return Err(invalid_value(path, "finite number", "1.0..=20000.0"));
            }
            Ok(ValidatedCornerValue::VelocityMaxPxS(value))
        }
        "input.corners.affordance" => {
            let Some(value) = value.as_bool() else {
                return Err(invalid_value(path, "bool", "true|false"));
            };
            Ok(ValidatedCornerValue::Affordance(value))
        }
        DISCOVERY_PATH => {
            let Some(value) = value.as_bool() else {
                return Err(invalid_value(path, "bool", "true|false"));
            };
            Ok(ValidatedCornerValue::Discovery(value))
        }
        _ if path.starts_with("input.corners.") => Err(SetValidationError::UnknownPath),
        _ if known_read_only_path(path) => Err(SetValidationError::ReadOnly),
        _ => Err(SetValidationError::UnknownPath),
    }
}

fn apply_corner_value(
    config: &mut CornerConfig,
    value: ValidatedCornerValue,
) -> (PropValue, PropValue) {
    match value {
        ValidatedCornerValue::Enabled(value) => {
            let old = config.enabled;
            config.enabled = value;
            (PropValue::Bool(old), PropValue::Bool(value))
        }
        ValidatedCornerValue::DeadzonePx(value) => {
            let old = config.deadzone_px;
            config.deadzone_px = value;
            (PropValue::F64(old), PropValue::F64(value))
        }
        ValidatedCornerValue::DwellMs(value) => {
            let old = config.dwell_ms;
            config.dwell_ms = value;
            (PropValue::U64(old), PropValue::U64(value))
        }
        ValidatedCornerValue::VelocityMaxPxS(value) => {
            let old = config.velocity_max_px_s;
            config.velocity_max_px_s = value;
            (PropValue::F64(old), PropValue::F64(value))
        }
        ValidatedCornerValue::Affordance(value) => {
            let old = config.affordance;
            config.affordance = value;
            (PropValue::Bool(old), PropValue::Bool(value))
        }
        ValidatedCornerValue::Discovery(value) => {
            let old = config.discovery;
            config.discovery = value;
            (PropValue::Bool(old), PropValue::Bool(value))
        }
    }
}

fn finite_number(
    path: &str,
    value: &Value,
    expected: &'static str,
    range: &'static str,
) -> Result<f64, SetValidationError> {
    value
        .as_f64()
        .filter(|number| number.is_finite())
        .ok_or_else(|| invalid_value(path, expected, range))
}

fn invalid_value(path: &str, expected: &'static str, range: &'static str) -> SetValidationError {
    SetValidationError::InvalidValue {
        path: path.to_string(),
        expected,
        range,
    }
}

fn known_read_only_path(path: &str) -> bool {
    const ROOTS: &[&str] = &[
        "info",
        "outputs",
        "surfaces",
        "windows",
        "stack",
        "focus",
        "decoration",
        "bindings",
        "dmabuf",
        "port",
    ];
    #[cfg(feature = "xwayland")]
    if path == "xwayland" || path == "xwayland.persist_path" || path == "xwayland.display" {
        // The subtree object is read-only like "input", and so are its two
        // served-but-never-written leaves (they exist: a write is
        // `read_only`, not `unknown_path`, like every `surfaces.*` leaf);
        // the one writable leaf is routed before validation ever runs.
        return true;
    }
    // The subtree object and the row list; the three writable leaves are
    // routed before validation ever reaches here, and any other
    // `workspaces.*` spelling is unknown, not read-only.
    if path == "workspaces" || path == "workspaces.list" {
        return true;
    }
    path == "input"
        || path == "input.corners"
        || path == "input.host"
        || ROOTS
            .iter()
            .any(|root| path == *root || path.starts_with(&format!("{root}.")))
}

#[cfg(test)]
mod tests {
    #[test]
    fn hold_acquire_release_round_trip() {
        let mut panels = BTreeMap::new();
        let request = |holder: &str, acquire: bool| PanelRequest::parse("comp.panel.hold", &json!({
            "output":"DP-1","edge":"left","surface":"quoin-panel-1",
            "holder":holder,"acquire":acquire,
        })).unwrap();
        for holder in ["pointer", "focus", "popup"] {
            let acquire = request(holder, true);
            assert_eq!(apply_panel_request(&mut panels, &acquire, Some(SurfaceId(7))), Some(true));
            assert_eq!(apply_panel_request(&mut panels, &acquire, Some(SurfaceId(7))), None, "idempotent");
            let release = request(holder, false);
            assert_eq!(apply_panel_request(&mut panels, &release, None), Some(false), "release after layer destruction");
            assert_eq!(apply_panel_request(&mut panels, &release, None), None);
        }
        assert_eq!(apply_panel_request(&mut panels, &request("focus", true), Some(SurfaceId(7))), Some(true));
        assert_eq!(apply_panel_request(&mut panels, &request("popup", true), Some(SurfaceId(7))), None);
        assert_eq!(apply_panel_request(&mut panels, &request("popup", false), None), None);
        assert_eq!(apply_panel_request(&mut panels, &request("focus", false), None), Some(false));
        for reveal in [true, false] {
            let record = ObservationRecord::PanelCommand {
                output: "DP-1".into(), edge: "left".into(), surface: "quoin-panel-1".into(),
                reveal, event_seq: 42,
            };
            assert_eq!(record.topic_suffix(), "panel.command");
            let wire = record.wire();
            let body: Value = serde_json::from_str(&wire.body).unwrap();
            assert_eq!(body["action"], if reveal { "reveal" } else { "conceal" });
            assert_eq!(body["surface"], "quoin-panel-1");
            assert_eq!(body["event_seq"], 42);
            let mut affected = AffectedTopics::default();
            affected.insert(record.topic_suffix());
            assert!(affected.contains("panel.command"));
        }
    }

    #[test]
    fn mode_report_updates_comp_state() {
        let mut panels = BTreeMap::new();
        let key = ("DP-1".to_owned(), "left".to_owned());
        // Revealed modes arrive with a live layer; a concealed (hidden) panel
        // has none, and its recreation carries a fresh token.
        for (mode, token, layer) in [
            ("pinned", "quoin-panel-1", Some(SurfaceId(3))),
            ("hidden", "quoin-panel-1", None),
            ("docked", "quoin-panel-2", Some(SurfaceId(4))),
            ("hidden", "quoin-panel-3", None),
        ] {
            let report = PanelRequest::parse("comp.panel.mode", &json!({
                "output":"DP-1","edge":"left","surface":token,"mode":mode,
            })).unwrap();
            // A hidden report re-states comp's verdict (nothing holds here);
            // persistent modes have no holders and draw no command.
            assert_eq!(
                apply_panel_request(&mut panels, &report, layer),
                (mode == "hidden").then_some(false)
            );
            let hold = PanelRequest::parse("comp.panel.hold", &json!({
                "output":"DP-1","edge":"left","surface":"menu-1","holder":"popup","acquire":true,
            })).unwrap();
            let revealed = apply_panel_request(&mut panels, &hold, Some(SurfaceId(7)));
            let panel = &panels[&key];
            assert_eq!(panel.mode, mode);
            assert_eq!(panel.surface, token, "the mode report rebinds the token");
            assert_eq!(panel.id, layer, "the mode report records its layer");
            assert_eq!(panel.held.is_empty(), mode != "hidden", "persistent modes ignore holds");
            assert_eq!(revealed, (mode == "hidden").then_some(true));
            let release = PanelRequest::parse("comp.panel.hold", &json!({
                "output":"DP-1","edge":"left","surface":"menu-1","holder":"popup","acquire":false,
            })).unwrap();
            apply_panel_request(&mut panels, &release, None);
        }
        // A persistent mode clears holds already recorded under hidden.
        let hold = PanelRequest::parse("comp.panel.hold", &json!({
            "output":"DP-1","edge":"left","surface":"menu-2","holder":"popup","acquire":true,
        })).unwrap();
        assert_eq!(apply_panel_request(&mut panels, &hold, Some(SurfaceId(8))), Some(true));
        let pin = PanelRequest::parse("comp.panel.mode", &json!({
            "output":"DP-1","edge":"left","surface":"quoin-panel-3","mode":"pinned",
        })).unwrap();
        apply_panel_request(&mut panels, &pin, Some(SurfaceId(5)));
        assert!(panels[&key].held.is_empty());
        // A release for an edge comp never saw invents no state.
        let stray = PanelRequest::parse("comp.panel.hold", &json!({
            "output":"DP-1","edge":"top","surface":"menu-9","holder":"popup","acquire":false,
        })).unwrap();
        assert_eq!(apply_panel_request(&mut panels, &stray, None), None);
        assert!(!panels.contains_key(&("DP-1".to_owned(), "top".to_owned())));
    }

    #[test]
    fn panel_request_refusals_name_the_offending_argument() {
        let base = json!({"output":"DP-1","edge":"left","surface":"quoin-panel-1"});
        let with = |extra: Value| {
            let mut args = base.clone();
            args.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            args
        };
        for (verb, args, field) in [
            ("comp.panel.hold", with(json!({"holder":"typo","acquire":true})), "holder"),
            ("comp.panel.hold", with(json!({"holder":"popup"})), "acquire"),
            ("comp.panel.hold", with(json!({"holder":"popup","acquire":true,"mode":"hidden"})), "mode"),
            ("comp.panel.hold", with(json!({"holder":"popup","acquire":true,"sticky":true})), "sticky"),
            ("comp.panel.mode", with(json!({"mode":"revealed"})), "mode"),
            ("comp.panel.mode", with(json!({"mode":"hidden","edge":"middle"})), "edge"),
            ("comp.panel.mode", with(json!({"mode":"hidden","surface":""})), "surface"),
            ("comp.panel.mode", with(json!({"mode":"hidden","acquire":false})), "acquire"),
            ("comp.panel.mode", json!([1]), "args"),
            ("comp.panel.hold", with(json!({"output":7,"holder":"popup","acquire":true})), "output"),
            ("comp.panel.hold", with(json!({"holder":"popup","acquire":"yes"})), "acquire"),
            ("comp.panel.hold", with(json!({"holder":["popup"],"acquire":true})), "holder"),
            ("comp.panel.mode", json!({"output":"DP-1","edge":"left","mode":"hidden"}), "surface"),
            ("comp.panel.mode", json!({"output":"DP-1","surface":"s","mode":"hidden"}), "edge"),
        ] {
            let Err(reply) = PanelRequest::parse(verb, &args) else {
                panic!("{verb} {args} must be refused");
            };
            let (rc, body) = reply.into_wire();
            let body: Value = serde_json::from_str(&body).unwrap();
            assert_eq!(rc, 10);
            assert_eq!(body["error"], "invalid_args");
            assert_eq!(body["field"], field, "{verb} {args}");
            assert_eq!(body["allowed"], json!(PANEL_ARGS));
        }
    }

    /// A hidden panel as Quoin reports it, with the pointer engaged on its
    /// hotspot (so its first verdict, a reveal, is already out).
    fn dwelled_panel(at: Instant) -> PanelHolders {
        let mut panel = PanelHolders::new("quoin-panel-1".into(), Some(SurfaceId(3)));
        assert_eq!(panel.settle(true), Some(false));
        panel.observe(Membership { dwelled: true, hotspot: true, ..Membership::default() }, at);
        assert_eq!(panel.settle(false), Some(true), "dwelling acquires the pointer");
        panel
    }

    fn popup(panel: &mut PanelHolders, acquire: bool) {
        if acquire {
            panel.held.insert("popup".into(), ("menu-1".into(), SurfaceId(9)));
        } else {
            panel.held.remove("popup");
        }
    }

    #[test]
    fn conceal_arms_only_on_last_holder_release() {
        let start = Instant::now();
        let ms = |n: u64| start + Duration::from_millis(n);
        let mut panel = dwelled_panel(ms(0));
        popup(&mut panel, true);
        // The pointer leaves while the popup still holds: no deadline.
        panel.observe(Membership::default(), ms(100));
        assert_eq!(panel.pointer, PointerHold::Lingering(ms(100)));
        assert_eq!(panel.conceal_deadline(), None, "the pointer is not the last holder");
        assert_eq!(panel.settle(false), None);
        // The popup releases inside the pointer's delay: the pointer is now
        // the last holder, and its deadline is where its own delay ends.
        popup(&mut panel, false);
        panel.observe(Membership::default(), ms(300));
        panel.expire(ms(300));
        assert_eq!(panel.conceal_deadline(), Some(ms(900)));
        assert_eq!(panel.settle(false), None, "still held until the deadline");
        panel.expire(ms(899));
        assert_eq!(panel.settle(false), None);
        panel.expire(ms(900));
        assert_eq!(panel.settle(false), Some(false), "the deadline conceals");
        assert_eq!(panel.conceal_deadline(), None, "and nothing re-arms");
        // A popup released after the pointer's delay ran out conceals at once.
        let mut panel = dwelled_panel(ms(0));
        popup(&mut panel, true);
        panel.observe(Membership::default(), ms(100));
        panel.expire(ms(2000));
        assert_eq!(panel.conceal_deadline(), None);
        assert_eq!(panel.settle(false), None);
        popup(&mut panel, false);
        assert_eq!(panel.settle(false), Some(false), "a popup's release is immediate");
        // Persistent panels arm nothing whatever the pointer does.
        let mut panel = dwelled_panel(ms(0));
        panel.mode = "pinned".into();
        panel.observe(Membership::default(), ms(100));
        assert_eq!(panel.conceal_deadline(), None);
        assert_eq!(panel.settle(false), None);
    }

    #[test]
    fn reentry_within_delay_cancels_conceal() {
        let start = Instant::now();
        let ms = |n: u64| start + Duration::from_millis(n);
        let mut panel = dwelled_panel(ms(0));
        panel.observe(Membership::default(), ms(100));
        assert_eq!(panel.conceal_deadline(), Some(ms(900)));
        // Back into the hotspot, not yet dwelled: re-entry keeps the hold.
        panel.observe(Membership { hotspot: true, ..Membership::default() }, ms(500));
        assert_eq!(panel.pointer, PointerHold::Inside);
        assert_eq!(panel.conceal_deadline(), None, "re-entry cancels the deadline");
        panel.expire(ms(2000));
        assert_eq!(panel.settle(false), None);
        // Into the visible panel itself: the same.
        panel.observe(Membership::default(), ms(2000));
        panel.observe(Membership { surface: true, ..Membership::default() }, ms(2700));
        assert_eq!(panel.conceal_deadline(), None);
        panel.expire(ms(5000));
        assert_eq!(panel.settle(false), None);
        // A new departure starts a fresh delay.
        panel.observe(Membership::default(), ms(5000));
        assert_eq!(panel.conceal_deadline(), Some(ms(5800)));
        panel.expire(ms(5800));
        assert_eq!(panel.settle(false), Some(false));
        // Once released, crossing the hotspot without dwelling acquires nothing.
        panel.observe(Membership { hotspot: true, ..Membership::default() }, ms(6000));
        assert_eq!(panel.pointer, PointerHold::Out);
        assert_eq!(panel.settle(false), None);
    }

    #[test]
    fn focus_holder_survives_pointer_departure() {
        let start = Instant::now();
        let ms = |n: u64| start + Duration::from_millis(n);
        let mut panel = dwelled_panel(ms(0));
        // A click inside the panel moved keyboard focus there.
        panel.observe(Membership { surface: true, focused: true, ..Membership::default() }, ms(50));
        // The pointer drifts away while the user types.
        panel.observe(Membership { focused: true, ..Membership::default() }, ms(100));
        assert_eq!(panel.conceal_deadline(), None, "focus holds: no timer");
        panel.expire(ms(10_000));
        assert_eq!(panel.pointer, PointerHold::Out);
        assert_eq!(panel.settle(false), None, "focus alone keeps the reveal");
        // Focus moving elsewhere is deliberate: the conceal is immediate.
        panel.observe(Membership::default(), ms(10_000));
        assert_eq!(panel.conceal_deadline(), None);
        assert_eq!(panel.settle(false), Some(false));
        // A hidden mode report re-states the verdict even when unchanged.
        assert_eq!(panel.settle(true), Some(false));
    }

    #[test]
    fn panel_surface_resolution_is_order_independent() {
        let (a, b) = (SurfaceId(1), SurfaceId(2));
        assert_eq!(resolve_panel_surface(std::iter::empty()), Ok(None));
        assert_eq!(resolve_panel_surface([(a, true)]), Ok(Some(a)));
        assert_eq!(resolve_panel_surface([(a, false)]), Err("panel_output_mismatch"));
        // A token on two layers is refused whichever one iteration meets
        // first, including when only one of them is on the right output.
        for pair in [[(a, true), (b, false)], [(b, false), (a, true)], [(a, true), (b, true)]] {
            assert_eq!(resolve_panel_surface(pair), Err("ambiguous_panel_surface"));
        }
    }

    /// A conceal that ends a commanded reveal arms the recorded-set check; a
    /// reveal lifts everything owed or enforced; a probe timing out stalls
    /// the owner (its popup and focus holds end); a mode report lifts the
    /// exclusion; the end of an incarnation takes everything with it.
    #[test]
    fn owed_conceals_follow_commanded_reveals_and_stall_drops_popup_and_focus() {
        let mut panel = PanelHolders::new("quoin-panel-1".into(), Some(SurfaceId(3)));
        let settle = |panel: &mut PanelHolders, restate: bool| {
            let previous = panel.verdict;
            let verdict = panel.settle(restate);
            if let Some(reveal) = verdict {
                panel.note_verdict(reveal, previous);
            }
            verdict
        };
        // A first (re-stated) conceal is not a commanded one.
        assert_eq!(settle(&mut panel, true), Some(false));
        assert!(!panel.arm_pending);
        // Comp reveals, then conceals: the showing set is recorded next pass.
        let start = Instant::now();
        panel.observe(Membership { dwelled: true, hotspot: true, ..Membership::default() }, start);
        assert_eq!(settle(&mut panel, false), Some(true));
        panel.observe(Membership::default(), start);
        panel.expire(start + CONCEAL_DELAY);
        assert_eq!(settle(&mut panel, false), Some(false));
        assert!(panel.arm_pending);
        // Owed and enforced state; a reveal lifts all of it.
        panel.pending.insert(SurfaceId(3));
        panel.enforce_at = Some(start);
        panel.enforced.insert(SurfaceId(3));
        assert!(panel.owed());
        panel.note_verdict(true, Some(false));
        assert!(!panel.owed() && panel.pending.is_empty() && !panel.arm_pending);
        // Stalling drops the popup and focus holds, and only those.
        panel.held.insert("popup".into(), ("menu-1".into(), SurfaceId(9)));
        panel.held.insert("focus".into(), ("quoin-panel-1".into(), SurfaceId(3)));
        panel.held.insert("pointer".into(), ("quoin-panel-1".into(), SurfaceId(3)));
        panel.stall();
        assert!(panel.stalled);
        assert_eq!(panel.held.keys().collect::<Vec<_>>(), ["pointer"]);
        // A mode report lifts the exclusion (the caller re-arms a still owed
        // conceal); a persistent one also ends the holds.
        panel.enforced.insert(SurfaceId(3));
        panel.probe = Some(Probe { deadline: start, serials: Vec::new() });
        let key = ("DP-1".to_owned(), "left".to_owned());
        let mut panels = BTreeMap::from([(key.clone(), panel)]);
        let report = PanelRequest::parse("comp.panel.mode", &json!({
            "output":"DP-1","edge":"left","surface":"quoin-panel-1","mode":"hidden",
        })).unwrap();
        apply_panel_request(&mut panels, &report, Some(SurfaceId(3)));
        let panel = panels.get_mut(&key).unwrap();
        assert!(!panel.owed(), "the exclusion and the probe lift");
        assert!(panel.held.contains_key("pointer"), "a hidden report keeps holds");
        let pin = PanelRequest::parse("comp.panel.mode", &json!({
            "output":"DP-1","edge":"left","surface":"quoin-panel-1","mode":"pinned",
        })).unwrap();
        apply_panel_request(&mut panels, &pin, Some(SurfaceId(3)));
        let panel = panels.get_mut(&key).unwrap();
        assert!(panel.held.is_empty());
        // The owner's disconnect ends the incarnation.
        panel.mode = "hidden".into();
        panel.held.insert("focus".into(), ("quoin-panel-1".into(), SurfaceId(3)));
        panel.popups.insert(SurfaceId(9));
        panel.enforced.insert(SurfaceId(3));
        panel.quiet = true;
        panel.drop_incarnation();
        assert!(panel.held.is_empty() && panel.popups.is_empty() && !panel.stalled && !panel.quiet);
        assert!(!panel.owed() && panel.owner.is_none());
        // Bus-side cleanup ends holds only.
        panel.held.insert("focus".into(), ("quoin-panel-1".into(), SurfaceId(3)));
        panel.enforced.insert(SurfaceId(3));
        panel.drop_holds();
        assert!(panel.held.is_empty());
        assert_eq!(panel.enforced, BTreeSet::from([SurfaceId(3)]), "enforcement is Wayland-side");
    }

    /// Only a registered holder service's report adopts an unowned edge.
    #[test]
    fn claims_bind_only_what_the_holder_reported() {
        let adopt = |claim: Claim| claim.binds();
        assert!(!adopt(Claim::Unowned));
        assert!(!adopt(Claim::Refuse));
        let request = PanelRequest::parse("comp.panel.mode", &json!({
            "output":"DP-1","edge":"left","surface":"quoin-panel-1","mode":"hidden","generation":3,
        })).unwrap();
        assert_eq!((request.generation, request.sender.as_str()), (Some(3), ""), "the body never names the sender");
        let Err(reply) = PanelRequest::parse("comp.panel.hold", &json!({
            "output":"DP-1","edge":"left","surface":"quoin-panel-1","holder":"focus","acquire":true,
            "generation":3,
        })) else {
            panic!("a hold carries no generation");
        };
        let body: Value = serde_json::from_str(&reply.into_wire().1).unwrap();
        assert_eq!(body["field"], "generation");
    }

    use std::{
        alloc::{GlobalAlloc, Layout, System},
        cell::Cell,
    };

    mod set_request_validation {
        use super::super::*;
        use serde_json::json;

        #[test]
        fn window_band_path_parses_only_the_exact_writable_shape() {
            assert_eq!(parse_window_band_path("windows.s12.band"), Some(12));
            assert_eq!(parse_window_band_path("windows.s0.band"), Some(0));
            assert_eq!(parse_window_band_path("windows.s.band"), None);
            assert_eq!(
                parse_window_band_path("windows.s0007.band"),
                None,
                "leading zeros would alias the canonical s7 key"
            );
            assert_eq!(parse_window_band_path("windows.s01.band"), None);
            assert_eq!(parse_window_band_path("windows.s12.title"), None);
            assert_eq!(parse_window_band_path("windows.s1x2.band"), None);
            assert_eq!(parse_window_band_path("windows.12.band"), None);
            assert_eq!(parse_window_band_path("surfaces.s12.band"), None);
            assert_eq!(parse_window_band_path("windows.s12.band.extra"), None);
        }

        #[test]
        fn window_band_value_accepts_exactly_the_operator_bands() {
            assert_eq!(
                validate_window_band_value("windows.s1.band", &json!("bottom")),
                Ok(StackBand::Bottom)
            );
            assert_eq!(
                validate_window_band_value("windows.s1.band", &json!("normal")),
                Ok(StackBand::Normal)
            );
            for refused in [json!("top"), json!("overlay"), json!("lock"), json!(1)] {
                assert!(matches!(
                    validate_window_band_value("windows.s1.band", &refused),
                    Err(SetValidationError::InvalidValue { .. })
                ));
            }
        }

        #[test]
        fn ingress_gate_admits_every_writable_leaf_family() {
            for leaf in ["minimized", "maximized", "fullscreen"] {
                let path = format!("windows.s7.{leaf}");
                for value in [json!(true), json!(false)] {
                    assert!(validate_set_request(&path, &value).is_ok());
                }
                for value in [json!(0), json!("true"), Value::Null] {
                    assert!(matches!(validate_set_request(&path, &value), Err(SetValidationError::InvalidValue { .. })));
                }
            }
            assert!(validate_set_request("input.corners.enabled", &json!(true)).is_ok());
            assert!(validate_set_request("windows.s7.band", &json!("bottom")).is_ok());
            // Other window leaves stay read-only, unknown stays unknown.
            assert!(matches!(
                validate_set_request("windows.s7.title", &json!("x")),
                Err(SetValidationError::ReadOnly)
            ));
            assert!(matches!(
                validate_set_request("nonsense.path", &json!(true)),
                Err(SetValidationError::UnknownPath)
            ));
            // 0.59.0: the four workspace leaves admit an integer >= 1 (the
            // count/live bound is the service's), refuse 0 and non-integers
            // with invalid_value naming the range, and the row list and the
            // subtree object are read-only.
            for path in [
                "windows.s7.workspace",
                "workspaces.count",
                "workspaces.current",
                "workspaces.o_dp_1.current",
            ] {
                assert!(validate_set_request(path, &json!(2)).is_ok(), "{path}");
                for refused in [json!(0), json!("2"), json!(2.5), json!(-1), json!(true)] {
                    assert!(
                        matches!(
                            validate_set_request(path, &refused),
                            Err(SetValidationError::InvalidValue {
                                expected: "integer",
                                ..
                            })
                        ),
                        "{path} refuses {refused}"
                    );
                }
            }
            assert!(matches!(
                validate_set_request("workspaces.count", &json!(0)),
                Err(SetValidationError::InvalidValue {
                    range: "1..=16",
                    ..
                })
            ));
            assert!(matches!(
                validate_set_request("workspaces.current", &json!(0)),
                Err(SetValidationError::InvalidValue {
                    range: "1..=count",
                    ..
                })
            ));
            for path in ["workspaces.list", "workspaces"] {
                assert!(
                    matches!(
                        validate_set_request(path, &json!([])),
                        Err(SetValidationError::ReadOnly)
                    ),
                    "{path}"
                );
            }
            assert!(matches!(
                validate_set_request("workspaces.o_dp_1", &json!(1)),
                Err(SetValidationError::UnknownPath)
            ));
            assert_eq!(
                parse_workspaces_set_path("workspaces.o_dp_1.current"),
                Some(WorkspacesSetTarget::Current(Some("o_dp_1".into())))
            );
            assert_eq!(parse_workspaces_set_path("workspaces.o_.current"), None);
            assert_eq!(parse_workspaces_set_path("workspaces.o_a.b.current"), None);
            assert_eq!(parse_workspaces_set_path("workspaces.dp_1.current"), None);
        }

        /// The regression this gate refactor fixed: `xwayland.enabled` had a
        /// complete service arm that the wire-level gate could never reach,
        /// because the old ingress validation only knew corner paths.
        #[cfg(feature = "xwayland")]
        #[test]
        fn ingress_gate_admits_the_xwayland_leaf_it_previously_rejected() {
            assert!(validate_set_request("xwayland.enabled", &json!(false)).is_ok());
            assert!(matches!(
                validate_set_request("xwayland.enabled", &json!("nope")),
                Err(SetValidationError::InvalidValue { .. })
            ));
            // The two served, never-written leaves exist, so a write is
            // `read_only` (like `workspaces.list` and every `surfaces.*`
            // leaf), not `unknown_path`.
            for path in ["xwayland", "xwayland.persist_path", "xwayland.display"] {
                assert!(
                    matches!(
                        validate_set_request(path, &json!(":9")),
                        Err(SetValidationError::ReadOnly)
                    ),
                    "{path}"
                );
            }
            assert!(matches!(
                validate_set_request("xwayland.nope", &json!(1)),
                Err(SetValidationError::UnknownPath)
            ));
        }
    }

    use super::*;

    thread_local! {
        static TRACKED_ALLOCATIONS: Cell<Option<usize>> = const { Cell::new(None) };
    }

    struct TestAllocator;

    #[global_allocator]
    static TEST_ALLOCATOR: TestAllocator = TestAllocator;

    unsafe impl GlobalAlloc for TestAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let _ = TRACKED_ALLOCATIONS.try_with(|count| {
                if let Some(current) = count.get() {
                    count.set(Some(current.saturating_add(1)));
                }
            });
            // SAFETY: this wrapper preserves System's allocation contract.
            unsafe { System.alloc(layout) }
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            let _ = TRACKED_ALLOCATIONS.try_with(|count| {
                if let Some(current) = count.get() {
                    count.set(Some(current.saturating_add(1)));
                }
            });
            // SAFETY: this wrapper preserves System's allocation contract.
            unsafe { System.alloc_zeroed(layout) }
        }

        unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
            // SAFETY: pointer/layout came from this System-backed allocator.
            unsafe { System.dealloc(pointer, layout) }
        }

        unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
            let _ = TRACKED_ALLOCATIONS.try_with(|count| {
                if let Some(current) = count.get() {
                    count.set(Some(current.saturating_add(1)));
                }
            });
            // SAFETY: pointer/layout came from System and size is forwarded.
            unsafe { System.realloc(pointer, layout, size) }
        }
    }

    fn allocations_during(run: impl FnOnce()) -> usize {
        TRACKED_ALLOCATIONS.with(|count| {
            assert_eq!(count.replace(Some(0)), None);
        });
        run();
        TRACKED_ALLOCATIONS.with(|count| count.replace(None).expect("tracking was armed"))
    }

    #[test]
    fn bounded_single_lane_carries_every_evicted_record_and_counts_it_once() {
        let lost = Arc::new(AtomicU64::new(0));
        let (mut producer, outbox) = test_outbox(Arc::clone(&lost), 2);
        assert_eq!(outbox.records.capacity(), Some(2));
        for sequence in 1..=6 {
            if sequence % 2 == 0 {
                producer.offer(ObservationRecord::FocusChanged {
                    keyboard: Some(sequence),
                    previous: None,
                    exclusive_latch: None,
                    event_seq: sequence,
                });
            } else {
                producer.offer(ObservationRecord::PropsChanged {
                    path: "input.corners.enabled".into(),
                    old: PropValue::Bool(true),
                    new: PropValue::Bool(false),
                    unix_ms: 0,
                    cause: "props.set",
                    event_seq: sequence,
                });
            }
        }
        assert_eq!(lost.load(Ordering::Acquire), 4);
        let first = outbox.records.recv().expect("first survivor");
        let second = outbox.records.recv().expect("second survivor");
        assert_eq!(first.record.event_seq(), 5);
        assert_eq!(second.record.event_seq(), 6);
        let first_loss = first
            .preceding_loss
            .expect("loss rides with first survivor");
        let second_loss = second
            .preceding_loss
            .expect("loss rides with second survivor");
        assert_eq!(
            (first_loss.first_lost_seq, first_loss.last_lost_seq),
            (1, 3)
        );
        assert_eq!(
            (second_loss.first_lost_seq, second_loss.last_lost_seq),
            (2, 4)
        );
        assert_eq!(
            first_loss.topics.iter().collect::<Vec<_>>(),
            [PROPS_TOPIC_SUFFIX]
        );
        assert_eq!(
            second_loss.topics.iter().collect::<Vec<_>>(),
            [FOCUS_TOPIC_SUFFIX]
        );
        assert_eq!(first_loss.cause, LossCause::OutboxOverflow);
        assert_eq!(second_loss.cause, LossCause::OutboxOverflow);
    }

    #[test]
    fn carried_loss_is_folded_when_its_survivor_is_later_evicted() {
        let lost = Arc::new(AtomicU64::new(0));
        let (mut producer, outbox) = test_outbox(Arc::clone(&lost), 1);
        for event_seq in 1..=4 {
            producer.offer(ObservationRecord::FocusChanged {
                keyboard: Some(event_seq),
                previous: None,
                exclusive_latch: None,
                event_seq,
            });
        }
        let survivor = outbox.records.recv().expect("newest record survives");
        assert_eq!(survivor.record.event_seq(), 4);
        let loss = survivor
            .preceding_loss
            .expect("the whole carried chain rides with the survivor");
        assert_eq!((loss.first_lost_seq, loss.last_lost_seq), (1, 3));
        assert_eq!(loss.topics.iter().collect::<Vec<_>>(), [FOCUS_TOPIC_SUFFIX]);
        assert_eq!(loss.cause, LossCause::OutboxOverflow);
        assert_eq!(lost.load(Ordering::Acquire), 3);
    }

    #[test]
    fn publisher_loss_dominates_interval_merges_in_both_orders() {
        let record = ObservationRecord::FocusChanged {
            keyboard: Some(1),
            previous: None,
            exclusive_latch: None,
            event_seq: 1,
        };
        let overflow = LossInterval::from_record(&record, LossCause::OutboxOverflow);
        let publisher = LossInterval::from_record(&record, LossCause::PublisherLoss);

        let mut overflow_then_publisher = overflow;
        overflow_then_publisher.merge(publisher);
        let mut publisher_then_overflow = publisher;
        publisher_then_overflow.merge(overflow);
        assert_eq!(overflow_then_publisher.cause, LossCause::PublisherLoss);
        assert_eq!(publisher_then_overflow.cause, LossCause::PublisherLoss);
    }

    #[test]
    fn successful_overflow_and_carried_loss_offer_paths_are_allocation_free() {
        let lost = Arc::new(AtomicU64::new(0));
        let (mut producer, outbox) = test_outbox(Arc::clone(&lost), 1);
        let allocations = allocations_during(|| {
            for event_seq in 1..=1_024 {
                producer.offer(ObservationRecord::FocusChanged {
                    keyboard: Some(event_seq),
                    previous: None,
                    exclusive_latch: None,
                    event_seq,
                });
            }
        });

        assert_eq!(allocations, 0, "offer must remain allocation-free");
        assert_eq!(lost.load(Ordering::Acquire), 1_023);
        let survivor = outbox.records.recv().expect("one bounded-lane survivor");
        assert_eq!(survivor.record.event_seq(), 1_024);
        let loss = survivor.preceding_loss.expect("carried loss is retained");
        assert_eq!((loss.first_lost_seq, loss.last_lost_seq), (1, 1_023));
    }

    #[test]
    fn keyed_row_add_is_one_row_granular_change() {
        let row = OutputSnapshot {
            name: "nested".into(),
            default: true,
            x: 0,
            y: 0,
            width: 640,
            height: 480,
            scale: 1.0,
            refresh_mhz: 60_000,
            usable: crate::protocol::port_snapshot::RectSnapshot {
                x: 0.0,
                y: 0.0,
                width: 640.0,
                height: 480.0,
            },
            presentation: None,
        };
        // A read snapshot's row carries volatile presentation leaves; a row
        // event never does.
        let read_row = OutputSnapshot {
            presentation: Some(crate::protocol::port_snapshot::OutputPresentationSnapshot {
                clock_id: 1,
                flags: None,
                flags_mask: None,
                refresh_us: None,
                frames: 9,
                interval_p50_us: None,
                interval_p99_us: None,
                since_us: 0,
            }),
            ..row.clone()
        };
        let mut changes = PendingPropChanges::new();
        diff_output_row(
            "outputs.o_nested",
            None,
            Some(&read_row),
            "output.geometry",
            &mut changes,
        );
        assert_eq!(changes.len(), 1);
        assert_eq!(
            changes.remove("outputs.o_nested"),
            Some((
                PropValue::null(),
                PropValue::OutputRow(Box::new(row.clone())),
                "output.geometry"
            ))
        );
        let wire = ObservationRecord::PropsChanged {
            path: "outputs.o_nested".into(),
            old: PropValue::null(),
            new: PropValue::OutputRow(Box::new(row.clone())),
            unix_ms: 0,
            cause: "output.geometry",
            event_seq: 1,
        }
        .wire();
        let body = serde_json::from_str::<Value>(&wire.body).expect("row frame body");
        assert!(body["old"].is_null());
        assert_eq!(body["path"], "outputs.o_nested");
        assert_eq!(body["new"]["width"], 640);

        let surface = SurfaceSnapshot {
            occlusion: Default::default(),
            id: 7,
            role: "toplevel",
            mapped: true,
            visible: true,
            x: 1.0,
            y: 2.0,
            width: 3.0,
            height: 4.0,
            band: "normal",
            sequence: 1,
            tree_index: 0,
            parent: None,
            output: Some("o_nested".into()),
            title: Some(Arc::from("row")),
            app_id: None,
            focused: false,
            activated: false,
            maximized: false,
            fullscreen: false,
            minimized: false,
            workspace: Some(1),
            decoration: Some("server"),
            layer: None,
            foreign_id: Some("f_7".into()),
            generation: 1,
            window: Default::default(),
        };
        let window = project_window_row(&surface);
        let read_window = WindowSnapshot {
            presentation: Some(Default::default()),
            ..window.clone()
        };
        let mut keyed = PendingPropChanges::new();
        diff_surface_row(
            "surfaces.s7",
            None,
            Some(&surface),
            "wayland.map",
            &mut keyed,
        );
        diff_window_row(
            "windows.s7",
            None,
            Some(&read_window),
            "wayland.map",
            &mut keyed,
        );
        assert_eq!(
            keyed.keys().cloned().collect::<Vec<_>>(),
            ["surfaces.s7", "windows.s7"]
        );
        assert_eq!(
            keyed["windows.s7"].1,
            PropValue::WindowRow(Box::new(window.clone())),
            "the added row has no presentation leaves"
        );
        // Rows that differ only in presentation produce no change.
        let mut quiet = PendingPropChanges::new();
        diff_window_row(
            "windows.s7",
            Some(&window),
            Some(&read_window),
            "wayland.commit",
            &mut quiet,
        );
        diff_output_row(
            "outputs.o_nested",
            Some(&row),
            Some(&read_row),
            "output.geometry",
            &mut quiet,
        );
        assert!(quiet.is_empty(), "{quiet:?}");
        let mut removed = PendingPropChanges::new();
        diff_surface_row(
            "surfaces.s7",
            Some(&surface),
            None,
            "wayland.unmap",
            &mut removed,
        );
        assert!(matches!(
            removed.get("surfaces.s7"),
            Some((
                PropValue::SurfaceRow(_),
                PropValue::Null(()),
                "wayland.unmap"
            ))
        ));
    }

    #[test]
    fn property_reducer_coalesces_each_path_and_excludes_operational_leaves() {
        let mut pending = PendingPropChanges::new();
        queue_prop_change(
            &mut pending,
            "surfaces.s2.title".into(),
            prop_str("old"),
            prop_str("middle"),
            "wayland.map",
        );
        queue_prop_change(
            &mut pending,
            "surfaces.s2.title".into(),
            prop_str("middle"),
            prop_str("new"),
            "wayland.focus",
        );
        queue_prop_change(
            &mut pending,
            "port.event_seq".into(),
            PropValue::U64(1),
            PropValue::U64(2),
            "wayland.map",
        );
        for volatile in [
            "windows.s2.presentation.presented",
            "outputs.o_dp_1.presentation.frames",
            "sources.scene.revision",
            "sources.scene.presentation.presented",
        ] {
            queue_prop_change(
                &mut pending,
                volatile.into(),
                PropValue::U64(1),
                PropValue::U64(2),
                "frame",
            );
        }
        assert_eq!(
            pending.remove("surfaces.s2.title"),
            Some((prop_str("old"), prop_str("new"), "wayland.map"))
        );
        assert!(pending.is_empty());

        queue_prop_change(
            &mut pending,
            "focus.keyboard".into(),
            PropValue::null(),
            PropValue::U64(2),
            "wayland.focus",
        );
        queue_prop_change(
            &mut pending,
            "focus.keyboard".into(),
            PropValue::U64(2),
            PropValue::null(),
            "wayland.focus",
        );
        assert!(pending.is_empty());
    }

    #[test]
    fn exact_topic_commands_and_flat_bodies() {
        let record = ObservationRecord::SurfaceMapped {
            id: 7,
            role: "toplevel".into(),
            foreign_id: Some("f_7".into()),
            window: SurfaceEdgeWindow {
                generation: 3,
                app_id: Some("dev.cosmix.Probe".into()),
                title: None,
            },
            event_seq: 9,
        };
        let wire = record.wire();
        assert_eq!(record.topic_suffix(), SURFACE_MAPPED_TOPIC_SUFFIX);
        assert_eq!(wire.get("command"), Some(SURFACE_MAPPED_TOPIC_SUFFIX));
        assert_eq!(
            topic_name("comp-nested", record.topic_suffix()),
            "comp-nested.surface.mapped"
        );
        assert_eq!(
            serde_json::from_str::<Value>(&wire.body).unwrap(),
            json!({
                "id": 7,
                "role": "toplevel",
                "generation": 3,
                "app_id": "dev.cosmix.Probe",
                "title": null,
                "foreign_id": "f_7",
                "event_seq": 9,
            })
        );
    }

    #[test]
    fn corner_clicked_v2_wire_has_press_modifiers_even_when_empty() {
        for (button, modifiers) in [
            ("left", vec![]),
            ("left", vec!["shift", "ctrl", "alt", "super"]),
            ("right", vec![]),
        ] {
            let record = ObservationRecord::CornerClickedV2 {
                output: "o_nested".into(),
                corner: Corner::BottomRight,
                dwell_ms: 217,
                button,
                kind: "brief",
                modifiers: modifiers.clone(),
                event_seq: 42,
            };
            let wire = record.wire();
            assert_eq!(record.topic_suffix(), "corner.clicked.v2");
            assert_eq!(wire.get("command"), Some("corner.clicked.v2"));
            assert_eq!(wire.get("event_seq"), Some("42"));
            assert_eq!(
                serde_json::from_str::<Value>(&wire.body).unwrap(),
                json!({
                    "output": "o_nested", "corner": "br", "dwell_ms": 217,
                    "button": button, "kind": "brief", "modifiers": modifiers,
                    "event_seq": 42,
                })
            );
        }
    }

    #[test]
    fn corner_clicked_wire_matches_entered_and_left() {
        let clicked = ObservationRecord::CornerClicked {
            output: "o_nested".into(),
            corner: Corner::BottomRight,
            dwell_ms: 217,
            event_seq: 42,
        };
        let wire = clicked.wire();
        assert_eq!(wire.get("command"), Some("corner.clicked"));
        assert_eq!(wire.get("event_seq"), Some("42"));
        let body = serde_json::from_str::<Value>(&wire.body).unwrap();
        assert_eq!(
            body,
            json!({
                "output": "o_nested", "corner": "br", "dwell_ms": 217, "event_seq": 42,
            })
        );
        for record in [
            ObservationRecord::CornerEntered {
                output: "o_nested".into(),
                corner: Corner::BottomRight,
                dwell_ms: 217,
                event_seq: 42,
            },
            ObservationRecord::CornerLeft {
                output: "o_nested".into(),
                corner: Corner::BottomRight,
                dwell_ms: 217,
                event_seq: 42,
            },
        ] {
            assert_eq!(
                serde_json::from_str::<Value>(&record.wire().body).unwrap(),
                body
            );
        }
    }

    #[test]
    fn affected_topics_supports_every_topic_including_pointer() {
        let mut topics = AffectedTopics::default();
        for suffix in TOPIC_SUFFIXES {
            topics.insert(suffix);
        }
        assert_eq!(topics.0, (1u16 << TOPIC_SUFFIXES.len()) - 1);
        assert_eq!(topics.iter().collect::<Vec<_>>(), TOPIC_SUFFIXES);
        topics.remove(CORNER_CLICKED_TOPIC_SUFFIX);
        assert!(!topics.contains(CORNER_CLICKED_TOPIC_SUFFIX));
        let mut clicked = AffectedTopics::default();
        clicked.insert(CORNER_CLICKED_TOPIC_SUFFIX);
        topics.merge(clicked);
        assert_eq!(topics.0, (1u16 << TOPIC_SUFFIXES.len()) - 1);
        topics.remove(POINTER_TOPIC_SUFFIX);
        assert!(!topics.contains(POINTER_TOPIC_SUFFIX));
    }

    #[test]
    fn every_topic_uses_the_registered_service_and_an_unprefixed_command() {
        let records = [
            ObservationRecord::PropsChanged {
                path: "input.corners.dwell_ms".into(),
                old: PropValue::U64(200),
                new: PropValue::U64(250),
                unix_ms: 0,
                cause: "props.set",
                event_seq: 1,
            },
            ObservationRecord::SurfaceMapped {
                id: 1,
                role: "toplevel".into(),
                foreign_id: None,
                window: SurfaceEdgeWindow::default(),
                event_seq: 2,
            },
            ObservationRecord::SurfaceUnmapped {
                id: 1,
                role: "toplevel".into(),
                foreign_id: None,
                window: SurfaceEdgeWindow::default(),
                event_seq: 3,
            },
            ObservationRecord::FocusChanged {
                keyboard: Some(1),
                previous: None,
                exclusive_latch: None,
                event_seq: 4,
            },
            ObservationRecord::OutputChanged {
                output: "o_nested".into(),
                row: OutputSnapshot {
                    name: "nested".into(),
                    default: true,
                    x: 0,
                    y: 0,
                    width: 1,
                    height: 1,
                    scale: 1.0,
                    refresh_mhz: 60_000,
                    usable: crate::protocol::port_snapshot::RectSnapshot {
                        x: 0.0,
                        y: 0.0,
                        width: 1.0,
                        height: 1.0,
                    },
                    presentation: None,
                },
                event_seq: 5,
            },
            ObservationRecord::CornerEntered {
                output: "o_nested".into(),
                corner: Corner::TopLeft,
                dwell_ms: 200,
                event_seq: 6,
            },
            ObservationRecord::CornerLeft {
                output: "o_nested".into(),
                corner: Corner::TopLeft,
                dwell_ms: 200,
                event_seq: 7,
            },
            ObservationRecord::CornerClicked {
                output: "o_nested".into(),
                corner: Corner::TopLeft,
                dwell_ms: 200,
                event_seq: 8,
            },
        ];
        let suffixes = [
            "props.changed",
            "surface.mapped",
            "surface.unmapped",
            "focus.changed",
            "output.changed",
            "corner.entered",
            "corner.left",
            "corner.clicked",
        ];
        for (record, suffix) in records.iter().zip(suffixes) {
            assert_eq!(record.topic_suffix(), suffix);
            assert_eq!(record.wire().get("command"), Some(suffix));
            assert_eq!(
                topic_name("observer-test", suffix),
                format!("observer-test.{suffix}")
            );
        }
    }

    #[test]
    fn corner_property_validation_accepts_endpoints_and_rejects_wrong_json_types() {
        for (path, value) in [
            ("input.corners.enabled", json!(false)),
            ("input.corners.deadzone_px", json!(1.0)),
            ("input.corners.deadzone_px", json!(256.0)),
            ("input.corners.dwell_ms", json!(0)),
            ("input.corners.dwell_ms", json!(5_000)),
            ("input.corners.velocity_max_px_s", json!(1.0)),
            ("input.corners.velocity_max_px_s", json!(20_000.0)),
            ("input.corners.affordance", json!(false)),
            ("input.corners.discovery", json!(true)),
        ] {
            assert!(
                validate_corner_value(path, &value).is_ok(),
                "{path}={value}"
            );
        }
        for (path, value) in [
            ("input.corners.enabled", json!(1)),
            ("input.corners.deadzone_px", json!("12")),
            ("input.corners.dwell_ms", json!(1.5)),
            ("input.corners.dwell_ms", json!(-1)),
            ("input.corners.velocity_max_px_s", json!(null)),
            ("input.corners.affordance", json!(0)),
            ("input.corners.discovery", json!("true")),
        ] {
            assert!(matches!(
                validate_corner_value(path, &value),
                Err(SetValidationError::InvalidValue { .. })
            ));
        }
    }

    #[test]
    fn corner_hold_property_is_unknown() {
        for value in [json!(0), json!(500), json!(5_001), json!(1.5)] {
            assert_eq!(
                validate_corner_value("input.corners.hold_ms", &value),
                Err(SetValidationError::UnknownPath)
            );
            assert_eq!(
                validate_set_request("input.corners.hold_ms", &value),
                Err(SetValidationError::UnknownPath)
            );
        }
    }

    #[test]
    fn event_sequence_exhaustion_offers_max_once_then_stops() {
        let event_loop = smithay::reexports::calloop::EventLoop::<WaylandState>::try_new()
            .expect("test event loop");
        let lost = Arc::new(AtomicU64::new(0));
        let (producer, outbox) = outbox(lost);
        let watermark = Arc::new(AtomicU64::new(0));
        let mut state =
            ObservationState::new(producer, Arc::clone(&watermark), event_loop.handle());
        state.event_seq = u64::MAX - 1;
        state.offer(|event_seq| ObservationRecord::FocusChanged {
            keyboard: None,
            previous: None,
            exclusive_latch: None,
            event_seq,
        });
        state.offer(|event_seq| ObservationRecord::FocusChanged {
            keyboard: None,
            previous: None,
            exclusive_latch: None,
            event_seq,
        });
        assert_eq!(
            outbox
                .records
                .try_iter()
                .map(|record| record.record.event_seq())
                .collect::<Vec<_>>(),
            [u64::MAX]
        );
        assert_eq!(watermark.load(Ordering::Acquire), u64::MAX);
        assert!(state.event_seq_exhausted);
    }
}
