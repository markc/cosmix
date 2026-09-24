//! `comp.*` Bus citizen worker and its bounded protocol ingress.

use std::{
    collections::BTreeMap,
    fmt::Write as _,
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering},
        mpsc::{self, Receiver, TrySendError},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use cosmix_bus::bus::BusMessage;
use cosmix_client::{ConnState, SupervisedClient, SupervisedError};
use serde_json::{Value, json};
use smithay::reexports::calloop::channel;
use tokio::{
    sync::{Semaphore, mpsc as tokio_mpsc, watch},
    task::JoinSet,
};

use crate::{
    decoration::DecorationStartup,
    protocol::{
        port_observation, port_snapshot, presentation_stats::STATS_RING,
        window_control::WindowTargetError,
    },
};
use port_observation::{
    LossCause, LossInterval, ObservationOutbox, ObservationProducer, ObservationRecord, PropValue,
    SetValidationError,
};
use port_snapshot::{
    BROKER_CONNECTED, BROKER_RETRYING, CompSnapshot, MAX_REPLY_BODY_BYTES, MAX_REPLY_WIRE_BYTES,
    SnapshotContext, dispatch_read, error, too_large,
};

pub(crate) const PORT_QUEUE_CAPACITY: usize = 16;
const PORT_REPLY_CAPACITY: usize = 16;
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(2);
/// Verbs that reply after a wait (`comp.input.sequence`, and in step 8
/// `comp.window.wait` / `close {force}`) hold their own permits, so a few
/// long waits can never starve ordinary reads and controls.
const LONG_VERB_PERMITS: usize = 8;
/// The longest a long verb may run before its reply is due.
pub(crate) const LONG_VERB_MAX: Duration = Duration::from_secs(60);
/// Admission slack on top of a long verb's own deadline: the protocol
/// thread answers at the deadline, and the worker must still be listening.
const LONG_VERB_SLACK: Duration = Duration::from_secs(1);
const SEQUENCE_MAX_STEPS: usize = 256;
/// At most four events a character (Shift press, key press/release,
/// Shift release), so the largest text stays within one verb's event cap
/// and small enough to write to a client in one pass.
const TEXT_MAX_CHARS: usize = 256;
/// The most seat events one verb (a whole sequence included) may inject.
pub(crate) const MAX_EVENTS_PER_VERB: usize = 4096;
const REPLY_SEND_TIMEOUT: Duration = Duration::from_secs(2);
const PUBLISH_TIMEOUT: Duration = Duration::from_secs(2);
const GAP_RETRY_INITIAL: Duration = Duration::from_secs(1);
const GAP_RETRY_MAX: Duration = Duration::from_secs(30);
const PORT_SHUTDOWN_GRACE: Duration = Duration::from_millis(300);
const CLIENT_SHUTDOWN_BUDGET: Duration = Duration::from_millis(250);
const DEREGISTER_BUDGET: Duration = Duration::from_millis(200);
const CLOSE_BUDGET: Duration = Duration::from_millis(50);

/// noded's own props topic; its `services.registered` diffs carry the full
/// registration set, which is how comp sees a holder service leave the Bus.
const REGISTRY_TOPIC: &str = "noded.props.changed";

pub(crate) enum PortCommand {
    Panel(PortPanelRequest),
    /// The registered services, from a registry diff (full set, not a delta).
    ServicesLive(std::collections::BTreeSet<String>),
    Snapshot(PortRequest),
    Watch(PortReply),
    PointerWatch(PortReply),
    Set(PortSetRequest),
    Window(PortWindowRequest),
    Input(PortInputRequest),
    Long(PortLongRequest),
    WatchState { active: bool, order: u64 },
}

pub(crate) struct PortRequest {
    pub(crate) reply: tokio::sync::oneshot::Sender<Arc<CompSnapshot>>,
    /// The read's own path or prefix, so the snapshot can be scoped to it
    /// (`ReadScopes`); `None` asks for the whole tree.
    pub(crate) scope: Option<String>,
}

pub(crate) struct PortReply {
    pub(crate) order: u64,
    pub(crate) reply: tokio::sync::oneshot::Sender<ControlReply>,
}

pub(crate) struct PortPanelRequest {
    pub(crate) order: u64,
    pub(crate) op: port_observation::PanelRequest,
    pub(crate) reply: Option<tokio::sync::oneshot::Sender<ControlReply>>,
}

pub(crate) struct PortSetRequest {
    pub(crate) order: u64,
    pub(crate) path: String,
    pub(crate) value: Value,
    /// Optional role-generation fence for `windows.s<id>.*` writes.
    pub(crate) generation: Option<u64>,
    pub(crate) reply: Option<tokio::sync::oneshot::Sender<ControlReply>>,
}

/// What `comp.window.stats` / `.stats.reset` measure: a window, fenced by
/// its role generation, or a content source, optionally fenced by its
/// registration number.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum StatsTarget {
    Window {
        id: u64,
        generation: u64,
    },
    Source {
        id: String,
        registration: Option<u64>,
    },
}

/// A window-addressed verb. `{id, generation}` is always required when a
/// window is named; only `restore` may name none (most recently minimised).
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum WindowOp {
    Minimize {
        id: u64,
        generation: u64,
    },
    Restore {
        target: Option<(u64, u64)>,
    },
    /// Keyboard focus; `raise` also raises and retargets the pointer (the
    /// Alt+Tab activation).
    Focus {
        id: u64,
        generation: u64,
        raise: bool,
    },
    Raise {
        id: u64,
        generation: u64,
    },
    /// The polite close (xdg `close` / X11 `WM_DELETE_WINDOW`).
    Close {
        id: u64,
        generation: u64,
    },
    Place(PlaceSpec),
    /// Presentation statistics for one window or content source.
    Stats {
        target: StatsTarget,
        samples: usize,
    },
    /// Zero one window's, one source's, or every row's statistics.
    StatsReset {
        target: Option<StatsTarget>,
    },
    /// `comp.workspace.switch`: the output's current workspace (`None` =
    /// the default output). Names no window, but changes what is on
    /// screen, so a session lock refuses it like the rest.
    SwitchWorkspace {
        output: Option<String>,
        index: WorkspaceIndex,
        wrap: bool,
    },
    /// `comp.window.send_to_workspace`: move one window; `follow` also
    /// switches to it and activates the window.
    SendToWorkspace {
        id: u64,
        generation: u64,
        index: WorkspaceIndex,
        follow: bool,
    },
}

/// Where `comp.workspace.switch` / `send_to_workspace` aim: a 1-based
/// index, or one step relative to the current (switch) or the window's own
/// (send) workspace.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WorkspaceIndex {
    Absolute(u32),
    Next,
    Prev,
}

/// `comp.window.place`: output-local logical window-geometry coordinates.
/// An absent field keeps its current value.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PlaceSpec {
    pub(crate) id: u64,
    pub(crate) generation: u64,
    pub(crate) output: Option<String>,
    pub(crate) x: Option<f64>,
    pub(crate) y: Option<f64>,
    pub(crate) width: Option<i32>,
    pub(crate) height: Option<i32>,
}

/// What `comp.window.wait` waits for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WaitUntil {
    Mapped,
    Visible,
    Presented,
    Size { width: i32, height: i32 },
    Focused,
    Unmapped,
    Gone,
}

impl WaitUntil {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Mapped => "mapped",
            Self::Visible => "visible",
            Self::Presented => "presented",
            Self::Size { .. } => "size",
            Self::Focused => "focused",
            Self::Unmapped => "unmapped",
            Self::Gone => "gone",
        }
    }
}

/// Which window a wait is about: one `{id, generation?}`, or the first
/// window matching the name filters.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct WindowMatch {
    pub(crate) id: Option<u64>,
    pub(crate) generation: Option<u64>,
    pub(crate) app_id: Option<String>,
    pub(crate) title: Option<String>,
    pub(crate) title_contains: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct WaitSpec {
    pub(crate) window: WindowMatch,
    pub(crate) until: WaitUntil,
    pub(crate) timeout: Duration,
}

/// A parsed `comp.window.*` verb: answered in one pass, or long.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum WindowVerb {
    Op(WindowOp),
    Long(LongOp),
}

pub(crate) struct PortWindowRequest {
    pub(crate) order: u64,
    pub(crate) op: WindowOp,
    pub(crate) reply: Option<tokio::sync::oneshot::Sender<ControlReply>>,
}

/// Press, release, or both in one verb (`click` for buttons, `tap` for
/// keys).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PressAction {
    Press,
    Release,
    Both,
}

/// Where `comp.input.pointer.move` puts the pointer.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum PointerMoveTarget {
    /// Output-local logical coordinates; `None` is the default output.
    Output {
        output: Option<String>,
        x: f64,
        y: f64,
    },
    /// A relative device delta (accelerated == unaccelerated).
    Relative { dx: f64, dy: f64 },
    /// Window-local coordinates, relative to the window-geometry origin.
    Window {
        id: u64,
        generation: u64,
        x: f64,
        y: f64,
        require_hit: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ScrollSource {
    Wheel,
    Finger,
    Continuous,
}

/// A key named by XKB keysym (`"Return"`, `"a"`, `"Super_L"`) or by raw
/// evdev code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum KeySpec {
    Name(String),
    Evdev(u32),
}

/// One `comp.input.*` operation, parsed and bounded on the worker.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum InputOp {
    /// `corners: false` keeps the move from arming a hot corner.
    PointerMove {
        target: PointerMoveTarget,
        corners: bool,
    },
    PointerButton {
        button: u32,
        action: PressAction,
    },
    PointerScroll {
        dx: Option<f64>,
        dy: Option<f64>,
        source: ScrollSource,
        v120: (Option<i32>, Option<i32>),
    },
    Key {
        key: KeySpec,
        action: PressAction,
        /// Keysym names of modifiers held around the key.
        modifiers: Vec<KeySpec>,
    },
    Text(String),
    ReleaseAll,
}

impl InputOp {
    /// The most seat events this op can inject. `release_all` releases what
    /// is held, which earlier (capped) verbs bounded.
    pub(crate) fn event_bound(&self) -> usize {
        match self {
            Self::PointerMove { .. } | Self::PointerScroll { .. } | Self::ReleaseAll => 1,
            Self::PointerButton { .. } => 2,
            Self::Key { modifiers, .. } => 2 * (modifiers.len() + 2),
            Self::Text(text) => 4 * text.chars().count(),
        }
    }
}

pub(crate) struct PortInputRequest {
    pub(crate) order: u64,
    pub(crate) op: InputOp,
    pub(crate) reply: Option<tokio::sync::oneshot::Sender<ControlReply>>,
}

/// One step of `comp.input.sequence`: the delay runs before the step.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SequenceStep {
    pub(crate) verb: &'static str,
    pub(crate) op: InputOp,
    pub(crate) delay: Duration,
}

/// A verb whose reply waits on a timer or an edge.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum LongOp {
    RegionSelect {
        output: Option<String>,
        timeout: Duration,
    },
    Sequence(Vec<SequenceStep>),
    Wait(WaitSpec),
    /// Polite close now; if the same `{id, generation}` is still alive at
    /// the deadline, kill its client.
    ForceClose {
        id: u64,
        generation: u64,
        timeout: Duration,
    },
}

impl LongOp {
    /// When the protocol thread must have answered by.
    fn budget(&self) -> Duration {
        match self {
            // Reserve four seconds after interaction for an acknowledged clean
            // frame (55s + 4s = 59s), strictly inside LONG_VERB_MAX's 60s.
            Self::RegionSelect { timeout, .. } => {
                *timeout
                    + crate::protocol::region_selection::REGION_CLEANUP_BUDGET
                    + Duration::from_secs(1)
            }
            Self::Sequence(steps) => steps.iter().map(|step| step.delay).sum(),
            Self::Wait(spec) => spec.timeout,
            Self::ForceClose { timeout, .. } => *timeout,
        }
    }
}

pub(crate) struct PortLongRequest {
    pub(crate) order: u64,
    pub(crate) op: Option<LongOp>,
    pub(crate) reply: Option<tokio::sync::oneshot::Sender<ControlReply>>,
    /// The queue slot this request holds until the protocol thread has
    /// taken it: dropping it there frees the slot while the verb waits, so
    /// long waits never fill the bounded ingress.
    pub(crate) slot: Option<QueueSlot>,
    /// When the worker admitted it; the verb's deadline runs from here.
    pub(crate) admitted: std::time::Instant,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ControlReply {
    PointerWatch {
        topic: String,
        lease_ms: u64,
    },
    Watch {
        topic: String,
        event_seq: u64,
        lost_count: u64,
    },
    Set {
        path: String,
        old: PropValue,
        new: PropValue,
        /// Durability report for file-persisted leaves: `Some(false)` means
        /// the in-memory change and the changed event stand but the write
        /// to disk FAILED and the value will not survive restart — the
        /// reply must not claim an outcome it did not achieve. `None` for
        /// process-lifetime leaves (field absent on the wire, so their
        /// bodies are byte-identical to before this field existed).
        persisted: Option<bool>,
    },
    Validation(SetValidationError),
    /// A `comp.window.*` success.
    Window {
        id: u64,
        generation: u64,
        title: Option<Arc<str>>,
        app_id: Option<Arc<str>>,
        minimized: bool,
        changed: bool,
    },
    WindowTarget {
        id: u64,
        error: WindowTargetError,
    },
    NotFound {
        minimized_count: usize,
    },
    /// A verb argument the verb does not define (a typo must not be
    /// silently ignored, or `{"gen": 3}` would act unfenced).
    InvalidArgs {
        field: String,
        allowed: &'static [&'static str],
    },
    Locked,
    Busy,
    /// A verb's success body (rc 0).
    Body(Value),
    /// A refusal: `{"error": error, ...detail}` (rc 10). `detail` is an
    /// object or null.
    Refused {
        error: &'static str,
        detail: Value,
    },
}

/// Every refusal carries `error_code` beside `error` (the 0.58.x alias), so
/// Mix `send` hands a script the whole structured body.
pub(crate) fn with_error_code(rc: u8, body: Arc<str>) -> (u8, Arc<str>) {
    if rc == 0 {
        return (rc, body);
    }
    let Ok(Value::Object(mut fields)) = serde_json::from_str::<Value>(&body) else {
        return (rc, body);
    };
    if fields.contains_key("error_code") {
        return (rc, body);
    }
    let Some(code) = fields.get("error").filter(|code| code.is_string()).cloned() else {
        return (rc, body);
    };
    fields.insert("error_code".into(), code);
    (rc, Arc::from(Value::Object(fields).to_string()))
}

impl ControlReply {
    /// The reply body as a JSON value, `error_code` included.
    pub(crate) fn wire_json(self) -> Value {
        let (rc, body) = self.into_wire();
        let (_, body) = with_error_code(rc, body);
        serde_json::from_str(&body).unwrap_or(Value::Null)
    }

    pub(crate) fn refused(error: &'static str, detail: Value) -> Self {
        Self::Refused { error, detail }
    }

    pub(crate) fn into_wire(self) -> (u8, Arc<str>) {
        match self {
            Self::PointerWatch { topic, lease_ms } => (
                0,
                Arc::from(json!({"version":1,"topic":topic,"lease_ms":lease_ms}).to_string()),
            ),
            Self::Watch {
                topic,
                event_seq,
                lost_count,
            } => (
                0,
                Arc::from(
                    json!({
                        "topic": topic,
                        "event_seq": event_seq,
                        "lost_count": lost_count,
                    })
                    .to_string(),
                ),
            ),
            Self::Set {
                path,
                old,
                new,
                persisted,
            } => (
                0,
                Arc::from(
                    {
                        let mut body = json!({
                            "path": path,
                            "old": old.wire_value(),
                            "new": new.wire_value(),
                        });
                        if let Some(persisted) = persisted {
                            body["persisted"] = json!(persisted);
                        }
                        body
                    }
                    .to_string(),
                ),
            ),
            Self::Validation(SetValidationError::UnknownPath) => error("unknown_path"),
            Self::Validation(SetValidationError::ReadOnly) => error("read_only"),
            Self::Validation(SetValidationError::InvalidValue {
                path,
                expected,
                range,
            }) => (
                10,
                Arc::from(
                    json!({
                        "error": "invalid_value",
                        "path": path,
                        "expected": expected,
                        "range": range,
                    })
                    .to_string(),
                ),
            ),
            Self::Window {
                id,
                generation,
                title,
                app_id,
                minimized,
                changed,
            } => (
                0,
                Arc::from(
                    json!({
                        "id": id,
                        "generation": generation,
                        "title": title.as_deref(),
                        "app_id": app_id.as_deref(),
                        "minimized": minimized,
                        "changed": changed,
                    })
                    .to_string(),
                ),
            ),
            Self::WindowTarget { id, error } => {
                let body = match error {
                    WindowTargetError::UnknownWindow => {
                        json!({"error": "unknown_window", "id": id})
                    }
                    WindowTargetError::StaleTarget { requested, current } => json!({
                        "error": "stale_target",
                        "id": id,
                        "generation": requested,
                        "current": current,
                    }),
                    WindowTargetError::NotManaged => json!({"error": "not_managed", "id": id}),
                    WindowTargetError::NotMapped => json!({"error": "not_mapped", "id": id}),
                };
                (10, Arc::from(body.to_string()))
            }
            Self::NotFound { minimized_count } => (
                10,
                Arc::from(
                    json!({"error": "not_found", "minimized_count": minimized_count}).to_string(),
                ),
            ),
            Self::InvalidArgs { field, allowed } => (
                10,
                Arc::from(
                    json!({"error": "invalid_args", "field": field, "allowed": allowed})
                        .to_string(),
                ),
            ),
            Self::Locked => error("locked"),
            Self::Busy => error("busy"),
            Self::Body(body) => (0, Arc::from(body.to_string())),
            Self::Refused {
                error: code,
                detail,
            } => {
                let mut body = serde_json::Map::new();
                body.insert("error".into(), json!(code));
                if let Value::Object(fields) = detail {
                    for (name, value) in fields {
                        if name != "error" {
                            body.insert(name, value);
                        }
                    }
                }
                (10, Arc::from(Value::Object(body).to_string()))
            }
        }
    }
}

pub(crate) enum PortControl {
    Panel(PortPanelRequest),
    Watch(PortReply),
    PointerWatch(PortReply),
    Set(PortSetRequest),
    Window(PortWindowRequest),
    Input(PortInputRequest),
    Long(PortLongRequest),
    WatchState { active: bool, order: u64 },
}

impl PortControl {
    pub(crate) fn order(&self) -> u64 {
        match self {
            Self::Panel(request) => request.order,
            Self::Watch(request) => request.order,
            Self::PointerWatch(request) => request.order,
            Self::Set(request) => request.order,
            Self::Window(request) => request.order,
            Self::Input(request) => request.order,
            Self::Long(request) => request.order,
            Self::WatchState { order, .. } => *order,
        }
    }
}

#[derive(Clone)]
pub(crate) struct PortIngress {
    sender: channel::SyncSender<PortCommand>,
    queue_depth: Arc<AtomicUsize>,
    control_order: Arc<AtomicU64>,
    pending_idle_order: Arc<AtomicU64>,
    pending_active_order: Arc<AtomicU64>,
}

impl PortIngress {
    pub(crate) fn request_panel(&self, op: port_observation::PanelRequest) -> Result<ControlAdmission, ()> {
        let (reply, receive) = tokio::sync::oneshot::channel();
        self.admit(PortCommand::Panel(PortPanelRequest {
            order: self.next_control_order(), op, reply: Some(reply),
        }), receive)
    }
    /// Whole-tree snapshot; production reads go through the scoped form.
    #[cfg(test)]
    pub(crate) fn request_snapshot(&self) -> Result<SnapshotAdmission, ()> {
        self.request_snapshot_scoped(None)
    }

    /// A snapshot request that names the read's own path or prefix, so the
    /// merged `ReadScopes` can skip subtrees nobody asked for; `None` reads
    /// the whole tree.
    pub(crate) fn request_snapshot_scoped(
        &self,
        scope: Option<String>,
    ) -> Result<SnapshotAdmission, ()> {
        let (reply, receive) = tokio::sync::oneshot::channel();
        self.admit(PortCommand::Snapshot(PortRequest { reply, scope }), receive)
            .map(SnapshotAdmission)
    }

    pub(crate) fn request_watch(&self) -> Result<ControlAdmission, ()> {
        let (reply, receive) = tokio::sync::oneshot::channel();
        let order = self.next_control_order();
        self.admit(PortCommand::Watch(PortReply { order, reply }), receive)
    }
    pub(crate) fn request_pointer_watch(&self) -> Result<ControlAdmission, ()> {
        let (reply, receive) = tokio::sync::oneshot::channel();
        let order = self.next_control_order();
        self.admit(
            PortCommand::PointerWatch(PortReply { order, reply }),
            receive,
        )
    }

    #[cfg(test)]
    pub(crate) fn request_set(&self, path: String, value: Value) -> Result<ControlAdmission, ()> {
        self.request_set_fenced(path, value, None)
    }

    pub(crate) fn request_set_fenced(
        &self,
        path: String,
        value: Value,
        generation: Option<u64>,
    ) -> Result<ControlAdmission, ()> {
        let (reply, receive) = tokio::sync::oneshot::channel();
        self.admit(
            PortCommand::Set(PortSetRequest {
                order: self.next_control_order(),
                path,
                value,
                generation,
                reply: Some(reply),
            }),
            receive,
        )
    }

    pub(crate) fn request_window(&self, op: WindowOp) -> Result<ControlAdmission, ()> {
        let (reply, receive) = tokio::sync::oneshot::channel();
        self.admit(
            PortCommand::Window(PortWindowRequest {
                order: self.next_control_order(),
                op,
                reply: Some(reply),
            }),
            receive,
        )
    }

    pub(crate) fn request_input(&self, op: InputOp) -> Result<ControlAdmission, ()> {
        let (reply, receive) = tokio::sync::oneshot::channel();
        self.admit(
            PortCommand::Input(PortInputRequest {
                order: self.next_control_order(),
                op,
                reply: Some(reply),
            }),
            receive,
        )
    }

    /// Admit a long verb. The queue slot rides inside the command and is
    /// released when the protocol thread takes it, not when the reply
    /// arrives; the reply wait is bounded by the verb's own budget.
    pub(crate) fn request_long(&self, op: LongOp) -> Result<LongAdmission, ()> {
        let timeout = op.budget().min(LONG_VERB_MAX) + LONG_VERB_SLACK;
        let (reply, receive) = tokio::sync::oneshot::channel();
        let slot = self.reserve_slot()?;
        let command = PortCommand::Long(PortLongRequest {
            order: self.next_control_order(),
            op: Some(op),
            reply: Some(reply),
            slot: Some(slot),
            admitted: std::time::Instant::now(),
        });
        // A refused send drops the command, and with it the slot.
        self.sender.try_send(command).map_err(|_| ())?;
        Ok(LongAdmission { receive, timeout })
    }

    /// Best effort: a full queue drops the set, and the next registry diff or
    /// the holds' leases repair it.
    pub(crate) fn services_live(&self, live: std::collections::BTreeSet<String>) {
        if self.sender.try_send(PortCommand::ServicesLive(live)).is_err() {
            tracing::debug!("registry update dropped: compositor port queue full");
        }
    }

    pub(crate) fn set_watch_state(&self, active: bool) {
        let order = self.next_control_order();
        if let Err(TrySendError::Full(_)) = self
            .sender
            .try_send(PortCommand::WatchState { active, order })
        {
            if active {
                self.pending_active_order.fetch_max(order, Ordering::AcqRel);
            } else {
                self.pending_idle_order.fetch_max(order, Ordering::AcqRel);
            }
        }
    }

    fn next_control_order(&self) -> u64 {
        self.control_order
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                Some(current.saturating_add(1))
            })
            .unwrap_or(u64::MAX)
            .saturating_add(1)
    }

    fn admit<T>(
        &self,
        command: PortCommand,
        receive: tokio::sync::oneshot::Receiver<T>,
    ) -> Result<Admission<T>, ()> {
        let depth = self.reserve_slot()?;
        match self.sender.try_send(command) {
            Ok(()) => Ok(Admission { receive, depth }),
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => Err(()),
        }
    }

    fn reserve_slot(&self) -> Result<QueueSlot, ()> {
        let mut depth = self.queue_depth.load(Ordering::Acquire);
        loop {
            if depth >= PORT_QUEUE_CAPACITY {
                return Err(());
            }
            match self.queue_depth.compare_exchange_weak(
                depth,
                depth + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(QueueSlot(Arc::clone(&self.queue_depth))),
                Err(observed) => depth = observed,
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn depth_for_test(&self) -> usize {
        self.queue_depth.load(Ordering::Acquire)
    }
}

pub(crate) struct Admission<T> {
    receive: tokio::sync::oneshot::Receiver<T>,
    depth: QueueSlot,
}

pub(crate) struct LongAdmission {
    receive: tokio::sync::oneshot::Receiver<ControlReply>,
    timeout: Duration,
}

impl LongAdmission {
    pub(crate) async fn receive(self) -> Result<ControlReply, ()> {
        match tokio::time::timeout(self.timeout, self.receive).await {
            Ok(Ok(reply)) => Ok(reply),
            Ok(Err(_)) | Err(_) => Err(()),
        }
    }

    #[cfg(test)]
    pub(crate) fn timeout_for_test(&self) -> Duration {
        self.timeout
    }
}

impl<T> Admission<T> {
    pub(crate) async fn receive(self) -> Result<T, ()> {
        let Self { receive, depth } = self;
        let result = match tokio::time::timeout(SNAPSHOT_TIMEOUT, receive).await {
            Ok(Ok(snapshot)) => Ok(snapshot),
            Ok(Err(_)) | Err(_) => Err(()),
        };
        drop(depth);
        result
    }
}

pub(crate) struct SnapshotAdmission(Admission<Arc<CompSnapshot>>);

impl SnapshotAdmission {
    pub(crate) async fn receive(self) -> Result<Arc<CompSnapshot>, ()> {
        self.0.receive().await
    }
}

pub(crate) type ControlAdmission = Admission<ControlReply>;

pub(crate) struct PortProtocolWiring {
    pub(crate) source: channel::Channel<PortCommand>,
    pub(crate) context: Arc<SnapshotContext>,
    pub(crate) observation_producer: ObservationProducer,
}

#[cfg(test)]
pub(crate) fn test_wiring(
    context: Arc<SnapshotContext>,
) -> (PortProtocolWiring, PortIngress, ObservationOutbox) {
    let (sender, source) = channel::sync_channel(PORT_QUEUE_CAPACITY);
    let ingress = PortIngress {
        sender,
        queue_depth: context.queue_depth.clone(),
        control_order: Arc::new(AtomicU64::new(0)),
        pending_idle_order: context.pending_idle_order.clone(),
        pending_active_order: context.pending_active_order.clone(),
    };
    let (observation_producer, observations) =
        port_observation::outbox(Arc::clone(&context.lost_count));
    (
        PortProtocolWiring {
            source,
            context,
            observation_producer,
        },
        ingress,
        observations,
    )
}

#[cfg(test)]
pub(crate) fn test_wiring_with_observation_capacity(
    context: Arc<SnapshotContext>,
    capacity: usize,
) -> (PortProtocolWiring, PortIngress, ObservationOutbox) {
    let (sender, source) = channel::sync_channel(PORT_QUEUE_CAPACITY);
    let ingress = PortIngress {
        sender,
        queue_depth: context.queue_depth.clone(),
        control_order: Arc::new(AtomicU64::new(0)),
        pending_idle_order: context.pending_idle_order.clone(),
        pending_active_order: context.pending_active_order.clone(),
    };
    let (observation_producer, observations) =
        port_observation::test_outbox(Arc::clone(&context.lost_count), capacity);
    (
        PortProtocolWiring {
            source,
            context,
            observation_producer,
        },
        ingress,
        observations,
    )
}

/// One Bus command through the real dispatch boundary as the named service
/// receives it; `pump` runs the compositor cycle that answers it. Returns the
/// reply's rc and JSON body.
#[cfg(test)]
pub(crate) fn test_dispatch(
    ingress: &PortIngress,
    service: &str,
    verb: &str,
    args: Value,
    pump: impl FnOnce(),
) -> (u8, Value) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("test runtime");
    let _entered = runtime.enter();
    let mut responders = JoinSet::new();
    let permits = Arc::new(Semaphore::new(PORT_QUEUE_CAPACITY));
    let long_permits = Arc::new(Semaphore::new(LONG_VERB_PERMITS));
    let (reply_sender, mut replies) = tokio_mpsc::channel(1);
    let reply_timeouts = Arc::new(AtomicU64::new(0));
    let command = cosmix_client::IncomingCommand {
        from: "test-caller".into(),
        command: verb.into(),
        id: Some("0".into()),
        body: args.to_string(),
        args,
        headers: BTreeMap::new(),
    };
    dispatch_incoming(
        ingress,
        &mut responders,
        &permits,
        &long_permits,
        &reply_sender,
        &reply_timeouts,
        service,
        command,
    );
    pump();
    runtime.block_on(async {
        while responders.join_next().await.is_some() {}
    });
    let reply = replies.try_recv().expect("the command is answered");
    (reply.rc, serde_json::from_str(&reply.body).expect("reply body is JSON"))
}

pub(crate) struct PortStarter {
    service: String,
    noded_url: String,
    ingress: PortIngress,
    broker: Arc<AtomicU8>,
    reply_timeouts: Arc<AtomicU64>,
    publish_timeouts: Arc<AtomicU64>,
    observations: ObservationOutbox,
    observation_notifier: Arc<tokio::sync::Notify>,
    lost_count: Arc<AtomicU64>,
}

pub(crate) struct PortWorker {
    shutdown: watch::Sender<bool>,
    ingress: Option<PortIngress>,
    completion: Mutex<Receiver<()>>,
    thread: Option<JoinHandle<()>>,
}

pub(crate) fn prepare(
    service: String,
    backend: &'static str,
    decoration: &DecorationStartup,
) -> Result<(PortProtocolWiring, PortStarter), String> {
    validate_service_name(&service)?;
    let noded_url = cosmix_config::client_helpers::resolve_noded_url();
    let broker = Arc::new(AtomicU8::new(BROKER_RETRYING));
    let queue_depth = Arc::new(AtomicUsize::new(0));
    let reply_timeouts = Arc::new(AtomicU64::new(0));
    let publish_timeouts = Arc::new(AtomicU64::new(0));
    let event_seq = Arc::new(AtomicU64::new(0));
    let lost_count = Arc::new(AtomicU64::new(0));
    let pending_idle_order = Arc::new(AtomicU64::new(0));
    let pending_active_order = Arc::new(AtomicU64::new(0));
    let (observation_producer, observations) = port_observation::outbox(Arc::clone(&lost_count));
    let observation_notifier = observation_producer.notifier();
    let (sender, source) = channel::sync_channel(PORT_QUEUE_CAPACITY);
    let ingress = PortIngress {
        sender,
        queue_depth: queue_depth.clone(),
        control_order: Arc::new(AtomicU64::new(0)),
        pending_idle_order: pending_idle_order.clone(),
        pending_active_order: pending_active_order.clone(),
    };
    let build = cosmix_buildinfo::build_info!();
    let context = Arc::new(SnapshotContext {
        service: Arc::from(service.as_str()),
        version: Arc::from(build.version),
        backend,
        engine: "bevy-0.19/wgpu",
        instance: Arc::from(random_instance_id()?.as_str()),
        decoration_enabled: decoration.enabled,
        decoration_style: decoration.theme.style.name(),
        broker: broker.clone(),
        queue_depth,
        reply_timeouts: reply_timeouts.clone(),
        publish_timeouts: publish_timeouts.clone(),
        event_seq: event_seq.clone(),
        lost_count: lost_count.clone(),
        pending_idle_order,
        pending_active_order,
    });
    Ok((
        PortProtocolWiring {
            source,
            context,
            observation_producer,
        },
        PortStarter {
            service,
            noded_url,
            ingress,
            broker,
            reply_timeouts,
            publish_timeouts,
            observations,
            observation_notifier,
            lost_count,
        },
    ))
}

impl PortStarter {
    pub(crate) fn start(self) -> Result<PortWorker, String> {
        let (shutdown, shutdown_rx) = watch::channel(false);
        let (completion_tx, completion) = mpsc::sync_channel(1);
        let thread_ingress = self.ingress.clone();
        let service = self.service;
        let noded_url = self.noded_url;
        let broker = self.broker;
        let reply_timeouts = self.reply_timeouts;
        let publish_timeouts = self.publish_timeouts;
        let observations = self.observations;
        let observation_notifier = self.observation_notifier;
        let lost_count = self.lost_count;
        let thread = thread::Builder::new()
            .name("cosmix-comp-port".into())
            .spawn(move || {
                let _completion = CompletionOnDrop(completion_tx);
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        tracing::error!(%error, "failed to build compositor Bus runtime");
                        return;
                    }
                };
                let connect_service = service.clone();
                let connect_url = noded_url;
                runtime.block_on(worker_loop(
                    service,
                    thread_ingress,
                    broker,
                    reply_timeouts,
                    publish_timeouts,
                    observations,
                    observation_notifier,
                    lost_count,
                    shutdown_rx,
                    move || {
                        let service = connect_service.clone();
                        let url = connect_url.clone();
                        async move {
                            SupervisedClient::connect_options(&service, &url)
                                .fatal_on_registration_rejection(true)
                                .connect()
                                .await
                                .map_err(|error| classify_connect_error(&service, error))
                        }
                    },
                ));
            })
            .map_err(|error| format!("failed to spawn compositor Bus worker: {error}"))?;
        Ok(PortWorker {
            shutdown,
            ingress: Some(self.ingress),
            completion: Mutex::new(completion),
            thread: Some(thread),
        })
    }
}

impl PortWorker {
    pub(crate) fn begin_shutdown(&mut self) {
        let _ = self.shutdown.send(true);
        self.ingress.take();
    }

    pub(crate) fn finish(mut self) {
        self.begin_shutdown();
        let Some(thread) = self.thread.take() else {
            return;
        };
        let completion = self
            .completion
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match completion.recv_timeout(PORT_SHUTDOWN_GRACE) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                if thread.join().is_err() {
                    tracing::error!("compositor Bus worker panicked during shutdown");
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                tracing::warn!(
                    grace_ms = PORT_SHUTDOWN_GRACE.as_millis(),
                    "compositor Bus worker did not stop in time and was detached"
                );
                drop(thread);
            }
        }
    }
}

struct CompletionOnDrop(mpsc::SyncSender<()>);

impl Drop for CompletionOnDrop {
    fn drop(&mut self) {
        let _ = self.0.try_send(());
    }
}

/// One admitted ingress entry; dropping it frees the slot.
pub(crate) struct QueueSlot(Arc<AtomicUsize>);

impl Drop for QueueSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Debug)]
enum ConnectAttemptError {
    Retry(String),
    RegistrationRejected {
        service: String,
        rc: u8,
        message: String,
    },
}

enum ConnectOutcome<C> {
    Connected(C),
    RegistrationRejected {
        service: String,
        rc: u8,
        message: String,
    },
    Shutdown,
}

async fn connect_loop<F, Fut, C>(
    shutdown: &mut watch::Receiver<bool>,
    broker: &AtomicU8,
    mut connector: F,
    minimum_delay: Duration,
) -> ConnectOutcome<C>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<C, ConnectAttemptError>>,
{
    let mut attempt = 0_u32;
    loop {
        broker.store(BROKER_RETRYING, Ordering::Release);
        let result = tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return ConnectOutcome::Shutdown;
                }
                continue;
            }
            result = connector() => result,
        };
        match result {
            Ok(client) => return ConnectOutcome::Connected(client),
            Err(ConnectAttemptError::RegistrationRejected {
                service,
                rc,
                message,
            }) => {
                return ConnectOutcome::RegistrationRejected {
                    service,
                    rc,
                    message,
                };
            }
            Err(ConnectAttemptError::Retry(message)) => {
                tracing::debug!(attempt, error = %message, "compositor Bus connect failed; retrying");
                let exponential = 250_u64.saturating_mul(1_u64 << attempt.min(16)).min(30_000);
                let delay = minimum_delay.max(Duration::from_millis(exponential));
                attempt = attempt.saturating_add(1);
                tokio::select! {
                    changed = shutdown.changed() => {
                        if changed.is_err() || *shutdown.borrow() {
                            return ConnectOutcome::Shutdown;
                        }
                    }
                    _ = tokio::time::sleep(delay) => {}
                }
            }
        }
    }
}

type WorkerFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

trait WorkerClient: Send + Sync + 'static {
    fn incoming(&self) -> Option<tokio_mpsc::UnboundedReceiver<cosmix_client::IncomingCommand>>;
    fn state(&self) -> ConnState;
    fn subscribe_state(&self) -> watch::Receiver<ConnState>;
    fn respond_parts<'a>(&'a self, reply: &'a PendingReply)
    -> WorkerFuture<'a, Result<(), String>>;
    fn publish<'a>(
        &'a self,
        headers: &'a BTreeMap<String, String>,
        wire: &'a str,
    ) -> WorkerFuture<'a, Result<(), String>>;
    fn deregister(&self) -> WorkerFuture<'_, Result<(), String>>;
    fn close(&self) -> WorkerFuture<'_, ()>;
    /// Subscribe once; the supervised client replays it on every reconnect.
    fn subscribe_topic<'a>(&'a self, topic: &'a str) -> WorkerFuture<'a, Result<(), String>>;
}

impl WorkerClient for SupervisedClient {
    fn incoming(&self) -> Option<tokio_mpsc::UnboundedReceiver<cosmix_client::IncomingCommand>> {
        SupervisedClient::incoming(self)
    }

    fn state(&self) -> ConnState {
        SupervisedClient::state(self)
    }

    fn subscribe_state(&self) -> watch::Receiver<ConnState> {
        SupervisedClient::subscribe_state(self)
    }

    fn respond_parts<'a>(
        &'a self,
        reply: &'a PendingReply,
    ) -> WorkerFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.respond_parts(
                &reply.from,
                &reply.command,
                reply.id.as_deref(),
                reply.rc,
                &reply.body,
            )
            .await
            .map_err(|error| error.to_string())
        })
    }

    fn publish<'a>(
        &'a self,
        headers: &'a BTreeMap<String, String>,
        wire: &'a str,
    ) -> WorkerFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let (rc, body, _) = self
                .call_with_headers_raw("noded", "topic.publish", headers, wire)
                .await
                .map_err(|error| error.to_string())?;
            if rc == 0 {
                Ok(())
            } else {
                Err(format!("topic.publish rejected with rc {rc}: {body}"))
            }
        })
    }

    fn deregister(&self) -> WorkerFuture<'_, Result<(), String>> {
        Box::pin(async move {
            SupervisedClient::deregister(self)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn close(&self) -> WorkerFuture<'_, ()> {
        Box::pin(SupervisedClient::close(self))
    }

    fn subscribe_topic<'a>(&'a self, topic: &'a str) -> WorkerFuture<'a, Result<(), String>> {
        Box::pin(async move {
            SupervisedClient::subscribe_topic(self, topic)
                .await
                .map_err(|error| error.to_string())
        })
    }
}

struct PendingReply {
    from: String,
    command: String,
    id: Option<String>,
    rc: u8,
    body: Arc<str>,
}

impl PendingReply {
    fn new(command: cosmix_client::IncomingCommand, (rc, body): (u8, Arc<str>)) -> Self {
        let (rc, body) = with_error_code(rc, body);
        Self {
            from: command.from,
            command: command.command,
            id: command.id,
            rc,
            body,
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn worker_loop<F, Fut, C>(
    service: String,
    ingress: PortIngress,
    broker: Arc<AtomicU8>,
    reply_timeouts: Arc<AtomicU64>,
    publish_timeouts: Arc<AtomicU64>,
    observations: ObservationOutbox,
    observation_notifier: Arc<tokio::sync::Notify>,
    lost_count: Arc<AtomicU64>,
    mut shutdown: watch::Receiver<bool>,
    connector: F,
) where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<C, ConnectAttemptError>>,
    C: WorkerClient,
{
    let outcome = connect_loop(&mut shutdown, &broker, connector, Duration::ZERO).await;
    let client = match outcome {
        ConnectOutcome::Connected(client) => Arc::new(client),
        ConnectOutcome::RegistrationRejected {
            service,
            rc,
            message,
        } => {
            tracing::error!(service = %service, rc, %message, "Bus registration rejected; compositor continues without a port");
            return;
        }
        ConnectOutcome::Shutdown => return,
    };
    let Some(mut incoming) = client.incoming() else {
        tracing::error!(service = %service, "compositor Bus incoming stream was already taken");
        return;
    };
    let mut states = client.subscribe_state();
    apply_connection_state(&broker, *states.borrow());
    let mut responders = JoinSet::new();
    let responder_permits = Arc::new(Semaphore::new(PORT_QUEUE_CAPACITY));
    let long_permits = Arc::new(Semaphore::new(LONG_VERB_PERMITS));
    let (reply_sender, reply_receiver) = tokio_mpsc::channel(PORT_REPLY_CAPACITY);
    let reply_task = tokio::spawn(reply_loop(
        Arc::clone(&client),
        Arc::from(service.as_str()),
        reply_receiver,
        Arc::clone(&reply_timeouts),
    ));
    let publisher_task = tokio::spawn(publisher_loop(
        Arc::clone(&client),
        Arc::from(service.as_str()),
        observations,
        Arc::clone(&observation_notifier),
        Arc::clone(&lost_count),
        Arc::clone(&publish_timeouts),
        shutdown.clone(),
    ));
    // Holder cleanup on Bus departure. Best effort: the holds' leases bound
    // what a missed departure can leave behind.
    let registry_task = tokio::spawn({
        let client = Arc::clone(&client);
        async move {
            if let Err(error) = client.subscribe_topic(REGISTRY_TOPIC).await {
                tracing::warn!(%error, "registry subscription failed; panel holds rely on their leases");
            }
        }
    });

    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
            changed = states.changed() => {
                if changed.is_err() {
                    break;
                }
                let state = *states.borrow_and_update();
                apply_connection_state(&broker, state);
                observation_notifier.notify_one();
                if state == ConnState::Fatal {
                    tracing::error!(service = %service, "Bus registration rejected during reconnect; compositor continues without a port");
                    break;
                }
            }
            command = incoming.recv() => {
                let Some(command) = command else {
                    let state = client.state();
                    apply_connection_state(&broker, state);
                    if state == ConnState::Fatal {
                        tracing::error!(service = %service, "Bus registration rejected during reconnect; compositor continues without a port");
                    }
                    break;
                };
                dispatch_incoming(
                    &ingress,
                    &mut responders,
                    &responder_permits,
                    &long_permits,
                    &reply_sender,
                    &reply_timeouts,
                    &service,
                    command,
                );
            }
            completed = responders.join_next(), if !responders.is_empty() => {
                if let Some(Err(error)) = completed {
                    tracing::debug!(%error, "compositor Bus responder task stopped");
                }
            }
        }
    }

    registry_task.abort();
    responders.abort_all();
    while responders.join_next().await.is_some() {}
    drop(reply_sender);
    reply_task.abort();
    let _ = reply_task.await;
    // Port shutdown is deliberately bounded: once requested, a retained gap
    // may be abandoned rather than extending compositor teardown indefinitely.
    publisher_task.abort();
    let _ = publisher_task.await;
    graceful_client_shutdown(client.as_ref()).await;
}

fn classify_connect_error(service: &str, error: SupervisedError) -> ConnectAttemptError {
    if let Some((rc, message)) = error.registration_rejection() {
        ConnectAttemptError::RegistrationRejected {
            service: service.to_string(),
            rc,
            message: message.to_string(),
        }
    } else {
        ConnectAttemptError::Retry(error.to_string())
    }
}

async fn graceful_client_shutdown<C: WorkerClient>(client: &C) {
    debug_assert_eq!(CLIENT_SHUTDOWN_BUDGET, DEREGISTER_BUDGET + CLOSE_BUDGET);
    match tokio::time::timeout(DEREGISTER_BUDGET, client.deregister()).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            tracing::debug!(%error, "compositor Bus deregister did not complete cleanly")
        }
        Err(_) => tracing::debug!(
            timeout_ms = DEREGISTER_BUDGET.as_millis(),
            "compositor Bus deregister timed out"
        ),
    }
    if tokio::time::timeout(CLOSE_BUDGET, client.close())
        .await
        .is_err()
    {
        tracing::debug!(
            timeout_ms = CLOSE_BUDGET.as_millis(),
            "compositor Bus close timed out"
        );
    }
}

fn apply_connection_state(broker: &AtomicU8, state: ConnState) {
    broker.store(
        if state == ConnState::Connected {
            BROKER_CONNECTED
        } else {
            BROKER_RETRYING
        },
        Ordering::Release,
    );
}

/// The pre-long-verb entry the existing tests drive: a fresh long pool per
/// call, so only the tests that exercise long verbs see pool pressure.
#[cfg(test)]
fn handle_incoming(
    ingress: &PortIngress,
    responders: &mut JoinSet<()>,
    responder_permits: &Arc<Semaphore>,
    reply_sender: &tokio_mpsc::Sender<PendingReply>,
    reply_timeouts: &Arc<AtomicU64>,
    service: &str,
    command: cosmix_client::IncomingCommand,
) {
    dispatch_incoming(
        ingress,
        responders,
        responder_permits,
        &Arc::new(Semaphore::new(LONG_VERB_PERMITS)),
        reply_sender,
        reply_timeouts,
        service,
        command,
    );
}

#[allow(clippy::too_many_arguments)]
fn dispatch_incoming(
    ingress: &PortIngress,
    responders: &mut JoinSet<()>,
    responder_permits: &Arc<Semaphore>,
    long_permits: &Arc<Semaphore>,
    reply_sender: &tokio_mpsc::Sender<PendingReply>,
    reply_timeouts: &Arc<AtomicU64>,
    service: &str,
    command: cosmix_client::IncomingCommand,
) {
    while let Some(completed) = responders.try_join_next() {
        if let Err(error) = completed {
            tracing::debug!(%error, "compositor Bus responder task stopped");
        }
    }
    let malformed =
        !command.body.is_empty() && serde_json::from_str::<Value>(&command.body).is_err();
    if command.from == "noded"
        && matches!(command.command.as_str(), "topic.active" | "topic.idle")
        && command.headers.iter().any(|(name, value)| {
            name.eq_ignore_ascii_case("name")
                && value
                    == &port_observation::topic_name(service, port_observation::PROPS_TOPIC_SUFFIX)
        })
    {
        ingress.set_watch_state(command.command == "topic.active");
        return;
    }
    // The broker's registry (only noded may publish its props topic): a
    // holder service that left takes its panel holds with it.
    if command.topic() == Some(REGISTRY_TOPIC) {
        if let Ok(body) = serde_json::from_str::<Value>(&command.body)
            && body["path"] == "services.registered"
            && let Some(live) = body["new"].as_array().and_then(|names| {
                names
                    .iter()
                    .map(|name| name.as_str().map(str::to_owned))
                    .collect::<Option<std::collections::BTreeSet<String>>>()
            })
        {
            ingress.services_live(live);
        }
        return;
    }
    if command.command == "comp.ping" {
        queue_reply(
            reply_sender,
            reply_timeouts,
            PendingReply::new(command, (0, Arc::from("{\"pong\":true}"))),
        );
        return;
    }
    if command.command == "comp.props.watch" || command.command == "comp.pointer.watch" {
        if malformed {
            queue_reply(
                reply_sender,
                reply_timeouts,
                PendingReply::new(command, error("unknown_path")),
            );
            return;
        }
        let permit = match Arc::clone(responder_permits).try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                queue_reply(
                    reply_sender,
                    reply_timeouts,
                    PendingReply::new(command, error("busy")),
                );
                return;
            }
        };
        let requested = if command.command == "comp.pointer.watch" {
            ingress.request_pointer_watch()
        } else {
            ingress.request_watch()
        };
        let admission = match requested {
            Ok(admission) => admission,
            Err(()) => {
                queue_reply(
                    reply_sender,
                    reply_timeouts,
                    PendingReply::new(command, error("busy")),
                );
                return;
            }
        };
        spawn_control_responder(
            responders,
            reply_sender,
            reply_timeouts,
            command,
            admission,
            permit,
        );
        return;
    }
    if command.command == "comp.props.set" {
        let parsed = if malformed {
            Err(invalid_set_shape(None))
        } else {
            parse_set(&command.args)
        };
        let (path, value, generation) = match parsed {
            Ok(parsed) => parsed,
            Err(reply) => {
                queue_reply(
                    reply_sender,
                    reply_timeouts,
                    PendingReply::new(command, reply),
                );
                return;
            }
        };
        if let Err(error) = port_observation::validate_set_request(&path, &value) {
            queue_reply(
                reply_sender,
                reply_timeouts,
                PendingReply::new(command, ControlReply::Validation(error).into_wire()),
            );
            return;
        }
        let permit = match Arc::clone(responder_permits).try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                queue_reply(
                    reply_sender,
                    reply_timeouts,
                    PendingReply::new(command, error("busy")),
                );
                return;
            }
        };
        let admission = match ingress.request_set_fenced(path, value, generation) {
            Ok(admission) => admission,
            Err(()) => {
                queue_reply(
                    reply_sender,
                    reply_timeouts,
                    PendingReply::new(command, error("busy")),
                );
                return;
            }
        };
        spawn_control_responder(
            responders,
            reply_sender,
            reply_timeouts,
            command,
            admission,
            permit,
        );
        return;
    }
    if let Some(verb) = window_verb(&command.command) {
        let parsed = if malformed {
            Err(invalid_argument("args", "JSON object", "{id, generation}"))
        } else {
            parse_window_verb(verb, &command.args)
        };
        let op = match parsed {
            Ok(WindowVerb::Op(op)) => op,
            Ok(WindowVerb::Long(op)) => {
                spawn_long_verb(
                    ingress,
                    responders,
                    long_permits,
                    reply_sender,
                    reply_timeouts,
                    command,
                    op,
                );
                return;
            }
            Err(reply) => {
                queue_reply(
                    reply_sender,
                    reply_timeouts,
                    PendingReply::new(command, reply.into_wire()),
                );
                return;
            }
        };
        let permit = match Arc::clone(responder_permits).try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                queue_reply(
                    reply_sender,
                    reply_timeouts,
                    PendingReply::new(command, error("busy")),
                );
                return;
            }
        };
        let admission = match ingress.request_window(op) {
            Ok(admission) => admission,
            Err(()) => {
                queue_reply(
                    reply_sender,
                    reply_timeouts,
                    PendingReply::new(command, error("busy")),
                );
                return;
            }
        };
        spawn_control_responder(
            responders,
            reply_sender,
            reply_timeouts,
            command,
            admission,
            permit,
        );
        return;
    }
    if command.command == "comp.region.select" {
        let parsed = if malformed {
            Err(invalid_argument(
                "args",
                "JSON object",
                "{output?, timeout_ms?}",
            ))
        } else {
            parse_region_select(&command.args)
        };
        match parsed {
            Ok(op) => spawn_long_verb(
                ingress,
                responders,
                long_permits,
                reply_sender,
                reply_timeouts,
                command,
                op,
            ),
            Err(reply) => queue_reply(
                reply_sender,
                reply_timeouts,
                PendingReply::new(command, reply.into_wire()),
            ),
        }
        return;
    }
    if command.command == "comp.input.sequence" {
        let parsed = if malformed {
            Err(invalid_argument(
                "args",
                "JSON object",
                "{steps, interval_ms?}",
            ))
        } else {
            parse_sequence(&command.args)
        };
        match parsed {
            Ok(op) => spawn_long_verb(
                ingress,
                responders,
                long_permits,
                reply_sender,
                reply_timeouts,
                command,
                op,
            ),
            Err(reply) => queue_reply(
                reply_sender,
                reply_timeouts,
                PendingReply::new(command, reply.into_wire()),
            ),
        }
        return;
    }
    if let Some(verb) = input_verb(&command.command) {
        let parsed = if malformed {
            Err(invalid_argument("args", "JSON object", "verb arguments"))
        } else {
            parse_input_op(verb, &command.args)
        };
        let op = match parsed {
            Ok(op) => op,
            Err(reply) => {
                queue_reply(
                    reply_sender,
                    reply_timeouts,
                    PendingReply::new(command, reply.into_wire()),
                );
                return;
            }
        };
        let permit = match Arc::clone(responder_permits).try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                queue_reply(
                    reply_sender,
                    reply_timeouts,
                    PendingReply::new(command, error("busy")),
                );
                return;
            }
        };
        let admission = match ingress.request_input(op) {
            Ok(admission) => admission,
            Err(()) => {
                queue_reply(
                    reply_sender,
                    reply_timeouts,
                    PendingReply::new(command, error("busy")),
                );
                return;
            }
        };
        spawn_control_responder(
            responders,
            reply_sender,
            reply_timeouts,
            command,
            admission,
            permit,
        );
        return;
    }
    // Literal `comp.*` commands addressed to the service, like every other
    // verb: `comp-nested.panel.hold` is an unknown verb, not an alias.
    if matches!(command.command.as_str(), "comp.panel.hold" | "comp.panel.mode") {
        let parsed = if malformed {
            Err(invalid_argument("args", "JSON object", "{output, edge, surface, ...}"))
        } else {
            port_observation::PanelRequest::parse(&command.command, &command.args)
        };
        let op = match parsed {
            Ok(mut op) => {
                // Only the broker's stamp names the holder service.
                op.sender.clone_from(&command.from);
                op
            }
            Err(reply) => {
                queue_reply(reply_sender, reply_timeouts,
                    PendingReply::new(command, reply.into_wire()));
                return;
            }
        };
        let permit = match Arc::clone(responder_permits).try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                queue_reply(reply_sender, reply_timeouts,
                    PendingReply::new(command, error("busy")));
                return;
            }
        };
        match ingress.request_panel(op) {
            Ok(admission) => spawn_control_responder(responders, reply_sender,
                reply_timeouts, command, admission, permit),
            Err(()) => queue_reply(reply_sender, reply_timeouts,
                PendingReply::new(command, error("busy"))),
        }
        return;
    }
    let needs_snapshot = matches!(
        command.command.as_str(),
        "comp.info"
            | "comp.props.get"
            | "comp.props.list"
            | "comp.props.describe"
            | "comp.windows.list"
    );
    if !needs_snapshot {
        queue_reply(
            reply_sender,
            reply_timeouts,
            PendingReply::new(command, error("unknown_verb")),
        );
        return;
    }
    if malformed {
        queue_reply(
            reply_sender,
            reply_timeouts,
            PendingReply::new(command, error("unknown_path")),
        );
        return;
    }
    let permit = match Arc::clone(responder_permits).try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            queue_reply(
                reply_sender,
                reply_timeouts,
                PendingReply::new(command, error("busy")),
            );
            return;
        }
    };
    // The read's own subtree scopes the snapshot (`read_scope`: info,
    // list's prefix, get/describe's path; windows.list has none = whole tree).
    let scope = read_scope(command.command.as_str(), &command.args);
    let admission = match ingress.request_snapshot_scoped(scope) {
        Ok(admission) => admission,
        Err(()) => {
            queue_reply(
                reply_sender,
                reply_timeouts,
                PendingReply::new(command, error("busy")),
            );
            drop(permit);
            return;
        }
    };
    let reply_sender = reply_sender.clone();
    let reply_timeouts = Arc::clone(reply_timeouts);
    responders.spawn(async move {
        let _permit = permit;
        let reply = match admission.receive().await {
            Ok(snapshot) => {
                dispatch_read(snapshot, command.command.clone(), command.args.clone()).await
            }
            Err(()) => error("busy"),
        };
        queue_reply(
            &reply_sender,
            &reply_timeouts,
            PendingReply::new(command, reply),
        );
    });
}

#[cfg(test)]
pub(crate) fn inject_topic_lifecycle_notice_for_test(
    ingress: &PortIngress,
    service: &str,
    active: bool,
) {
    let mut responders = JoinSet::new();
    let responder_permits = Arc::new(Semaphore::new(PORT_QUEUE_CAPACITY));
    let (reply_sender, _replies) = tokio_mpsc::channel(1);
    let reply_timeouts = Arc::new(AtomicU64::new(0));
    let mut headers = BTreeMap::new();
    headers.insert(
        "name".into(),
        port_observation::topic_name(service, port_observation::PROPS_TOPIC_SUFFIX),
    );
    handle_incoming(
        ingress,
        &mut responders,
        &responder_permits,
        &reply_sender,
        &reply_timeouts,
        service,
        cosmix_client::IncomingCommand {
            from: "noded".into(),
            command: if active {
                "topic.active".into()
            } else {
                "topic.idle".into()
            },
            id: None,
            args: Value::Null,
            body: String::new(),
            headers,
        },
    );
    debug_assert!(responders.is_empty());
}

type ParsedSet = (String, Value, Option<u64>);

fn parse_set(args: &Value) -> Result<ParsedSet, (u8, Arc<str>)> {
    let object = args.as_object().ok_or_else(|| invalid_set_shape(None))?;
    let path = object
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| error("unknown_path"))?;
    let value = object
        .get("value")
        .cloned()
        .ok_or_else(|| invalid_set_shape(Some(path)))?;
    let generation = match object.get("generation") {
        None | Some(Value::Null) => None,
        Some(generation) => Some(generation.as_u64().ok_or_else(|| {
            invalid_argument("generation", "unsigned integer", "windows.s<id>.generation")
                .into_wire()
        })?),
    };
    // The fence names a window, so it only means something on a window
    // leaf; anywhere else it is a mis-aimed request, not something to drop.
    if generation.is_some() && port_observation::parse_window_leaf_path(path).is_none() {
        return Err(invalid_argument(
            "generation",
            "absent",
            "generation applies to windows.s<id>.* paths only",
        )
        .into_wire());
    }
    Ok((path.to_string(), value, generation))
}

fn invalid_argument(path: &str, expected: &'static str, range: &'static str) -> ControlReply {
    ControlReply::Validation(SetValidationError::InvalidValue {
        path: path.to_string(),
        expected,
        range,
    })
}

fn window_arg(
    object: &serde_json::Map<String, Value>,
    name: &str,
) -> Result<Option<u64>, ControlReply> {
    match object.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| invalid_argument(name, "unsigned integer", "0..=u64::MAX")),
    }
}

/// `comp.window.minimize {id, generation}` and
/// `comp.window.restore {id?, generation?}` (both or neither).
fn parse_window_op(verb: &str, args: &Value) -> Result<WindowOp, ControlReply> {
    let empty = serde_json::Map::new();
    let object = match args {
        Value::Null => &empty,
        Value::Object(object) => object,
        _ => {
            return Err(invalid_argument("args", "JSON object", "{id, generation}"));
        }
    };
    const WINDOW_ARGS: &[&str] = &["id", "generation"];
    if let Some(field) = object
        .keys()
        .find(|field| !WINDOW_ARGS.contains(&field.as_str()))
    {
        return Err(ControlReply::InvalidArgs {
            field: field.clone(),
            allowed: WINDOW_ARGS,
        });
    }
    let id = window_arg(object, "id")?;
    let generation = window_arg(object, "generation")?;
    let target = match (id, generation) {
        (Some(id), Some(generation)) => Some((id, generation)),
        (None, None) => None,
        (Some(_), None) => {
            return Err(invalid_argument(
                "generation",
                "unsigned integer",
                "required with id (read windows.s<id>.generation)",
            ));
        }
        (None, Some(_)) => {
            return Err(invalid_argument(
                "id",
                "unsigned integer",
                "required with generation",
            ));
        }
    };
    if verb == "comp.window.minimize" {
        let Some((id, generation)) = target else {
            return Err(invalid_argument("id", "unsigned integer", "required"));
        };
        Ok(WindowOp::Minimize { id, generation })
    } else {
        Ok(WindowOp::Restore { target })
    }
}

const WINDOW_VERBS: &[&str] = &[
    "comp.window.minimize",
    "comp.window.restore",
    "comp.window.focus",
    "comp.window.raise",
    "comp.window.close",
    "comp.window.place",
    "comp.window.wait",
    "comp.window.stats",
    "comp.window.stats.reset",
    "comp.workspace.switch",
    "comp.window.send_to_workspace",
];

fn window_verb(verb: &str) -> Option<&'static str> {
    WINDOW_VERBS.iter().copied().find(|known| *known == verb)
}

/// The default and the ceiling for `comp.window.wait` and
/// `comp.window.close {force}`.
const WINDOW_WAIT_DEFAULT: Duration = Duration::from_secs(10);
const CLOSE_FORCE_DEFAULT: Duration = Duration::from_secs(3);

fn required_target(object: &serde_json::Map<String, Value>) -> Result<(u64, u64), ControlReply> {
    let id = window_arg(object, "id")?
        .ok_or_else(|| invalid_argument("id", "unsigned integer", "required"))?;
    let generation = window_arg(object, "generation")?.ok_or_else(|| {
        invalid_argument(
            "generation",
            "unsigned integer",
            "required (read windows.s<id>.generation)",
        )
    })?;
    Ok((id, generation))
}

fn bool_arg(
    object: &serde_json::Map<String, Value>,
    name: &'static str,
    default: bool,
) -> Result<bool, ControlReply> {
    match present(object, name) {
        None => Ok(default),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => Err(invalid_argument(name, "bool", "true|false")),
    }
}

/// `index` of the workspace verbs: an unsigned integer `>= 1`, `"next"` or
/// `"prev"`. `0` is refused here as `invalid_value` (the contract: outside
/// `1..count`, 0 included); an index above the count is refused by the
/// compositor, which knows the count.
fn workspace_index_arg(
    object: &serde_json::Map<String, Value>,
) -> Result<WorkspaceIndex, ControlReply> {
    match present(object, "index") {
        Some(Value::String(step)) if step == "next" => Ok(WorkspaceIndex::Next),
        Some(Value::String(step)) if step == "prev" => Ok(WorkspaceIndex::Prev),
        Some(value) if value.as_u64().is_some_and(|index| index >= 1) => value
            .as_u64()
            .and_then(|index| u32::try_from(index).ok())
            .map(WorkspaceIndex::Absolute)
            .ok_or_else(|| workspace_index_refusal("1..=workspaces.count|next|prev")),
        Some(_) => Err(workspace_index_refusal("1..=workspaces.count|next|prev")),
        None => Err(workspace_index_refusal(
            "required: 1..=workspaces.count|next|prev",
        )),
    }
}

fn workspace_index_refusal(range: &'static str) -> ControlReply {
    invalid_argument("index", "unsigned integer or next|prev", range)
}

/// `output` of a placement or a workspace switch: a non-empty `outputs`
/// key or output name.
fn output_arg(object: &serde_json::Map<String, Value>) -> Result<Option<String>, ControlReply> {
    match present(object, "output") {
        None => Ok(None),
        Some(Value::String(output)) if !output.is_empty() => Ok(Some(output.clone())),
        Some(_) => Err(invalid_argument(
            "output",
            "string",
            "outputs.<key> key or output name",
        )),
    }
}

fn size_arg(
    object: &serde_json::Map<String, Value>,
    name: &'static str,
) -> Result<Option<i32>, ControlReply> {
    match present(object, name) {
        None => Ok(None),
        Some(value) => value
            .as_u64()
            .filter(|size| (1..=32_767).contains(size))
            .map(|size| Some(size as i32))
            .ok_or_else(|| invalid_argument(name, "integer", "1..=32767")),
    }
}

fn string_arg(
    object: &serde_json::Map<String, Value>,
    name: &'static str,
) -> Result<Option<String>, ControlReply> {
    match present(object, name) {
        None => Ok(None),
        Some(Value::String(value)) if value.len() <= 4096 => Ok(Some(value.clone())),
        Some(_) => Err(invalid_argument(name, "string", "at most 4096 bytes")),
    }
}

fn timeout_arg(
    object: &serde_json::Map<String, Value>,
    default: Duration,
) -> Result<Duration, ControlReply> {
    match present(object, "timeout_ms") {
        None => Ok(default),
        Some(value) => value
            .as_u64()
            .filter(|ms| (1..=LONG_VERB_MAX.as_millis() as u64).contains(ms))
            .map(Duration::from_millis)
            .ok_or_else(|| invalid_argument("timeout_ms", "unsigned integer", "1..=60000")),
    }
}

/// Every `comp.window.*` verb. Minimise and restore keep their own parser.
pub(crate) fn parse_window_verb(verb: &str, args: &Value) -> Result<WindowVerb, ControlReply> {
    let empty = serde_json::Map::new();
    match verb {
        "comp.window.minimize" | "comp.window.restore" => {
            parse_window_op(verb, args).map(WindowVerb::Op)
        }
        "comp.window.stats" | "comp.window.stats.reset" => {
            parse_stats_op(verb, args).map(WindowVerb::Op)
        }
        "comp.window.focus" => {
            const ALLOWED: &[&str] = &["id", "generation", "raise"];
            let object = args_object(args, &empty, ALLOWED)?;
            let (id, generation) = required_target(object)?;
            Ok(WindowVerb::Op(WindowOp::Focus {
                id,
                generation,
                raise: bool_arg(object, "raise", true)?,
            }))
        }
        "comp.window.raise" => {
            const ALLOWED: &[&str] = &["id", "generation"];
            let object = args_object(args, &empty, ALLOWED)?;
            let (id, generation) = required_target(object)?;
            Ok(WindowVerb::Op(WindowOp::Raise { id, generation }))
        }
        "comp.window.close" => {
            const ALLOWED: &[&str] = &["id", "generation", "force", "timeout_ms"];
            let object = args_object(args, &empty, ALLOWED)?;
            let (id, generation) = required_target(object)?;
            if bool_arg(object, "force", false)? {
                Ok(WindowVerb::Long(LongOp::ForceClose {
                    id,
                    generation,
                    timeout: timeout_arg(object, CLOSE_FORCE_DEFAULT)?,
                }))
            } else if present(object, "timeout_ms").is_some() {
                Err(invalid_argument(
                    "timeout_ms",
                    "absent",
                    "timeout_ms applies with force:true only",
                ))
            } else {
                Ok(WindowVerb::Op(WindowOp::Close { id, generation }))
            }
        }
        "comp.window.place" => {
            const ALLOWED: &[&str] = &["id", "generation", "output", "x", "y", "width", "height"];
            let object = args_object(args, &empty, ALLOWED)?;
            let (id, generation) = required_target(object)?;
            let output = output_arg(object)?;
            let spec = PlaceSpec {
                id,
                generation,
                x: finite_arg(object, "x")?,
                y: finite_arg(object, "y")?,
                width: size_arg(object, "width")?,
                height: size_arg(object, "height")?,
                output,
            };
            if spec.output.is_none()
                && spec.x.is_none()
                && spec.y.is_none()
                && spec.width.is_none()
                && spec.height.is_none()
            {
                return Err(invalid_argument(
                    "x",
                    "finite number",
                    "place needs at least one of output, x, y, width, height",
                ));
            }
            Ok(WindowVerb::Op(WindowOp::Place(spec)))
        }
        "comp.workspace.switch" => {
            const ALLOWED: &[&str] = &["index", "output", "wrap"];
            let object = args_object(args, &empty, ALLOWED)?;
            Ok(WindowVerb::Op(WindowOp::SwitchWorkspace {
                index: workspace_index_arg(object)?,
                output: output_arg(object)?,
                wrap: bool_arg(object, "wrap", true)?,
            }))
        }
        "comp.window.send_to_workspace" => {
            const ALLOWED: &[&str] = &["id", "generation", "index", "follow"];
            let object = args_object(args, &empty, ALLOWED)?;
            let (id, generation) = required_target(object)?;
            Ok(WindowVerb::Op(WindowOp::SendToWorkspace {
                id,
                generation,
                index: workspace_index_arg(object)?,
                follow: bool_arg(object, "follow", false)?,
            }))
        }
        "comp.window.wait" => {
            const ALLOWED: &[&str] = &["match", "until", "width", "height", "timeout_ms"];
            const MATCH: &[&str] = &["id", "generation", "app_id", "title", "title_contains"];
            let object = args_object(args, &empty, ALLOWED)?;
            let filters = match present(object, "match") {
                Some(Value::Object(filters)) => filters,
                _ => {
                    return Err(invalid_argument(
                        "match",
                        "object",
                        "{id?, generation?, app_id?, title?, title_contains?}",
                    ));
                }
            };
            if let Some(field) = filters
                .keys()
                .find(|field| !MATCH.contains(&field.as_str()))
            {
                return Err(ControlReply::InvalidArgs {
                    field: format!("match.{field}"),
                    allowed: MATCH,
                });
            }
            let window = WindowMatch {
                id: window_arg(filters, "id")?,
                generation: window_arg(filters, "generation")?,
                app_id: string_arg(filters, "app_id")?,
                title: string_arg(filters, "title")?,
                title_contains: string_arg(filters, "title_contains")?,
            };
            if window.id.is_some()
                && (window.app_id.is_some()
                    || window.title.is_some()
                    || window.title_contains.is_some())
            {
                return Err(invalid_argument(
                    "match",
                    "object",
                    "match by id or by app_id/title/title_contains, not both",
                ));
            }
            if window.generation.is_some() && window.id.is_none() {
                return Err(invalid_argument(
                    "match.id",
                    "unsigned integer",
                    "required with generation",
                ));
            }
            if window == WindowMatch::default() {
                return Err(invalid_argument(
                    "match",
                    "object",
                    "at least one of id, app_id, title, title_contains",
                ));
            }
            let width = size_arg(object, "width")?;
            let height = size_arg(object, "height")?;
            let until = match present(object, "until").and_then(Value::as_str) {
                Some("mapped") => WaitUntil::Mapped,
                Some("visible") => WaitUntil::Visible,
                Some("presented") => WaitUntil::Presented,
                Some("size") => match (width, height) {
                    (Some(width), Some(height)) => WaitUntil::Size { width, height },
                    _ => {
                        return Err(invalid_argument(
                            "width",
                            "integer",
                            "until:size needs width and height",
                        ));
                    }
                },
                Some("focused") => WaitUntil::Focused,
                Some("unmapped") => WaitUntil::Unmapped,
                Some("gone") => WaitUntil::Gone,
                _ => {
                    return Err(invalid_argument(
                        "until",
                        "string",
                        "mapped|visible|presented|size|focused|unmapped|gone",
                    ));
                }
            };
            if !matches!(until, WaitUntil::Size { .. }) && (width.is_some() || height.is_some()) {
                return Err(invalid_argument(
                    "width",
                    "absent",
                    "width and height apply to until:size only",
                ));
            }
            Ok(WindowVerb::Long(LongOp::Wait(WaitSpec {
                window,
                until,
                timeout: timeout_arg(object, WINDOW_WAIT_DEFAULT)?,
            })))
        }
        _ => Err(invalid_argument("verb", "window verb", "comp.window.*")),
    }
}

const INPUT_VERBS: &[&str] = &[
    "comp.input.pointer.move",
    "comp.input.pointer.button",
    "comp.input.pointer.scroll",
    "comp.input.key",
    "comp.input.release_all",
];

/// The canonical `&'static` name of a single-step input verb.
fn input_verb(verb: &str) -> Option<&'static str> {
    INPUT_VERBS.iter().copied().find(|known| *known == verb)
}

/// evdev `BTN_LEFT` / `BTN_RIGHT` / `BTN_MIDDLE`.
pub(crate) const BTN_LEFT: u32 = 0x110;
pub(crate) const BTN_RIGHT: u32 = 0x111;
pub(crate) const BTN_MIDDLE: u32 = 0x112;
/// evdev `KEY_MAX`: every key and button code is at most this.
const EVDEV_CODE_MAX: u64 = 0x2ff;

fn args_object<'a>(
    args: &'a Value,
    empty: &'a serde_json::Map<String, Value>,
    allowed: &'static [&'static str],
) -> Result<&'a serde_json::Map<String, Value>, ControlReply> {
    let object = match args {
        Value::Null => empty,
        Value::Object(object) => object,
        _ => return Err(invalid_argument("args", "JSON object", "verb arguments")),
    };
    if let Some(field) = object
        .keys()
        .find(|field| !allowed.contains(&field.as_str()))
    {
        return Err(ControlReply::InvalidArgs {
            field: field.clone(),
            allowed,
        });
    }
    Ok(object)
}

fn present<'a>(object: &'a serde_json::Map<String, Value>, name: &str) -> Option<&'a Value> {
    object.get(name).filter(|value| !value.is_null())
}

fn finite_arg(
    object: &serde_json::Map<String, Value>,
    name: &'static str,
) -> Result<Option<f64>, ControlReply> {
    match present(object, name) {
        None => Ok(None),
        Some(value) => value
            .as_f64()
            .filter(|value| value.is_finite() && value.abs() <= 1.0e6)
            .map(Some)
            .ok_or_else(|| invalid_argument(name, "finite number", "-1e6..=1e6")),
    }
}

fn required_finite(
    object: &serde_json::Map<String, Value>,
    name: &'static str,
) -> Result<f64, ControlReply> {
    finite_arg(object, name)?.ok_or_else(|| invalid_argument(name, "finite number", "required"))
}

fn press_action(
    object: &serde_json::Map<String, Value>,
    both: &'static str,
) -> Result<PressAction, ControlReply> {
    match present(object, "action") {
        None => Ok(PressAction::Both),
        Some(Value::String(action)) if action == "press" => Ok(PressAction::Press),
        Some(Value::String(action)) if action == "release" => Ok(PressAction::Release),
        Some(Value::String(action)) if action == both => Ok(PressAction::Both),
        Some(_) => Err(invalid_argument(
            "action",
            "string",
            if both == "click" {
                "press|release|click"
            } else {
                "press|release|tap"
            },
        )),
    }
}

fn evdev_code(value: &Value, name: &'static str, minimum: u64) -> Result<u32, ControlReply> {
    value
        .as_u64()
        .filter(|code| (minimum..=EVDEV_CODE_MAX).contains(code))
        .map(|code| code as u32)
        .ok_or_else(|| invalid_argument(name, "evdev code", "an evdev code up to 0x2ff"))
}

fn key_spec(value: &Value, name: &'static str) -> Result<KeySpec, ControlReply> {
    match value {
        Value::String(key) if !key.is_empty() && key.len() <= 64 => Ok(KeySpec::Name(key.clone())),
        Value::Number(_) => evdev_code(value, name, 1).map(KeySpec::Evdev),
        _ => Err(invalid_argument(
            name,
            "keysym name or evdev code",
            "XKB keysym name (\"Return\", \"a\") or 1..=0x2ff",
        )),
    }
}

fn modifier_spec(value: &Value) -> Result<KeySpec, ControlReply> {
    let name = match value.as_str() {
        Some("shift") => "Shift_L",
        Some("ctrl" | "control") => "Control_L",
        Some("alt") => "Alt_L",
        Some("super" | "logo") => "Super_L",
        Some("altgr") => "ISO_Level3_Shift",
        _ => {
            return Err(invalid_argument(
                "modifiers",
                "list of modifier names",
                "shift|ctrl|alt|super|altgr",
            ));
        }
    };
    Ok(KeySpec::Name(name.into()))
}

/// Parse one `comp.input.*` verb's arguments. Shared by the direct verbs
/// and `comp.input.sequence` steps, so a step is exactly the verb.
pub(crate) fn parse_input_op(verb: &str, args: &Value) -> Result<InputOp, ControlReply> {
    let empty = serde_json::Map::new();
    match verb {
        "comp.input.pointer.move" => {
            const ALLOWED: &[&str] = &[
                "output",
                "x",
                "y",
                "dx",
                "dy",
                "window",
                "require_hit",
                "corners",
            ];
            let object = args_object(args, &empty, ALLOWED)?;
            let corners = bool_arg(object, "corners", true)?;
            let moved = |target| Ok(InputOp::PointerMove { target, corners });
            let require_hit = match present(object, "require_hit") {
                None => None,
                Some(Value::Bool(value)) => Some(*value),
                Some(_) => return Err(invalid_argument("require_hit", "bool", "true|false")),
            };
            let relative = present(object, "dx").is_some() || present(object, "dy").is_some();
            if let Some(window) = present(object, "window") {
                const WINDOW: &[&str] = &["id", "generation"];
                let window = match window {
                    Value::Object(window) => window,
                    _ => return Err(invalid_argument("window", "object", "{id, generation}")),
                };
                if let Some(field) = window
                    .keys()
                    .find(|field| !WINDOW.contains(&field.as_str()))
                {
                    return Err(ControlReply::InvalidArgs {
                        field: format!("window.{field}"),
                        allowed: WINDOW,
                    });
                }
                if relative || present(object, "output").is_some() {
                    return Err(invalid_argument(
                        "window",
                        "exclusive form",
                        "{window, x, y} takes no output, dx or dy",
                    ));
                }
                let id = window_arg(window, "id")?
                    .ok_or_else(|| invalid_argument("window.id", "unsigned integer", "required"))?;
                let generation = window_arg(window, "generation")?.ok_or_else(|| {
                    invalid_argument(
                        "window.generation",
                        "unsigned integer",
                        "required (read windows.s<id>.generation)",
                    )
                })?;
                return moved(PointerMoveTarget::Window {
                    id,
                    generation,
                    x: required_finite(object, "x")?,
                    y: required_finite(object, "y")?,
                    require_hit: require_hit.unwrap_or(false),
                });
            }
            if require_hit.is_some() {
                return Err(invalid_argument(
                    "require_hit",
                    "absent",
                    "require_hit applies to the {window, x, y} form only",
                ));
            }
            if relative {
                if present(object, "x").is_some()
                    || present(object, "y").is_some()
                    || present(object, "output").is_some()
                {
                    return Err(invalid_argument(
                        "dx",
                        "exclusive form",
                        "{dx, dy} takes no output, x or y",
                    ));
                }
                return moved(PointerMoveTarget::Relative {
                    dx: finite_arg(object, "dx")?.unwrap_or(0.0),
                    dy: finite_arg(object, "dy")?.unwrap_or(0.0),
                });
            }
            let output = match present(object, "output") {
                None => None,
                Some(Value::String(output)) if !output.is_empty() => Some(output.clone()),
                Some(_) => {
                    return Err(invalid_argument(
                        "output",
                        "string",
                        "outputs.<key> key or output name",
                    ));
                }
            };
            moved(PointerMoveTarget::Output {
                output,
                x: required_finite(object, "x")?,
                y: required_finite(object, "y")?,
            })
        }
        "comp.input.pointer.button" => {
            const ALLOWED: &[&str] = &["button", "action"];
            let object = args_object(args, &empty, ALLOWED)?;
            let button = match present(object, "button") {
                None => BTN_LEFT,
                Some(Value::String(name)) if name == "left" => BTN_LEFT,
                Some(Value::String(name)) if name == "right" => BTN_RIGHT,
                Some(Value::String(name)) if name == "middle" => BTN_MIDDLE,
                Some(value @ Value::Number(_)) => evdev_code(value, "button", 0x100)?,
                Some(_) => {
                    return Err(invalid_argument(
                        "button",
                        "button name or evdev code",
                        "left|right|middle|0x100..=0x2ff",
                    ));
                }
            };
            Ok(InputOp::PointerButton {
                button,
                action: press_action(object, "click")?,
            })
        }
        "comp.input.pointer.scroll" => {
            const ALLOWED: &[&str] = &["dx", "dy", "source", "v120"];
            let object = args_object(args, &empty, ALLOWED)?;
            let dx = finite_arg(object, "dx")?;
            let dy = finite_arg(object, "dy")?;
            if dx.is_none() && dy.is_none() {
                return Err(invalid_argument(
                    "dy",
                    "finite number",
                    "dx or dy is required",
                ));
            }
            let source = match present(object, "source") {
                None => ScrollSource::Wheel,
                Some(Value::String(source)) if source == "wheel" => ScrollSource::Wheel,
                Some(Value::String(source)) if source == "finger" => ScrollSource::Finger,
                Some(Value::String(source)) if source == "continuous" => ScrollSource::Continuous,
                Some(_) => {
                    return Err(invalid_argument(
                        "source",
                        "string",
                        "wheel|finger|continuous",
                    ));
                }
            };
            let detent = |name: &'static str,
                          value: Option<&Value>,
                          amount: Option<f64>|
             -> Result<Option<i32>, ControlReply> {
                match value {
                    Some(value) => {
                        if amount.is_none() {
                            return Err(invalid_argument(
                                name,
                                "absent",
                                "a detent count needs the matching axis",
                            ));
                        }
                        value
                            .as_i64()
                            .and_then(|value| i32::try_from(value).ok())
                            .filter(|value| value.unsigned_abs() <= 120 * 1000)
                            .map(Some)
                            .ok_or_else(|| invalid_argument(name, "integer", "-120000..=120000"))
                    }
                    // A wheel reports detents: 15 logical units to one
                    // detent (120) is libinput's convention. Other sources
                    // have none, and an absent count stays absent.
                    None if source == ScrollSource::Wheel => {
                        Ok(amount.map(|amount| (amount * 8.0).round() as i32))
                    }
                    None => Ok(None),
                }
            };
            let v120 = match present(object, "v120") {
                None => (detent("v120.dx", None, dx)?, detent("v120.dy", None, dy)?),
                Some(Value::Object(v120)) => {
                    const V120: &[&str] = &["dx", "dy"];
                    if let Some(field) = v120.keys().find(|field| !V120.contains(&field.as_str())) {
                        return Err(ControlReply::InvalidArgs {
                            field: format!("v120.{field}"),
                            allowed: V120,
                        });
                    }
                    if source != ScrollSource::Wheel {
                        return Err(invalid_argument(
                            "v120",
                            "absent",
                            "detent counts belong to source wheel",
                        ));
                    }
                    (
                        detent("v120.dx", present(v120, "dx"), dx)?,
                        detent("v120.dy", present(v120, "dy"), dy)?,
                    )
                }
                Some(_) => return Err(invalid_argument("v120", "object", "{dx?, dy?}")),
            };
            Ok(InputOp::PointerScroll {
                dx,
                dy,
                source,
                v120,
            })
        }
        "comp.input.key" => {
            const ALLOWED: &[&str] = &["key", "action", "modifiers", "text"];
            let object = args_object(args, &empty, ALLOWED)?;
            if let Some(text) = present(object, "text") {
                if ["key", "action", "modifiers"]
                    .iter()
                    .any(|name| present(object, name).is_some())
                {
                    return Err(invalid_argument(
                        "text",
                        "exclusive form",
                        "{text} takes no key, action or modifiers",
                    ));
                }
                return match text {
                    Value::String(text)
                        if !text.is_empty() && text.chars().count() <= TEXT_MAX_CHARS =>
                    {
                        Ok(InputOp::Text(text.clone()))
                    }
                    _ => Err(invalid_argument("text", "string", "1..=256 characters")),
                };
            }
            let key = present(object, "key")
                .ok_or_else(|| invalid_argument("key", "keysym name or evdev code", "required"))
                .and_then(|key| key_spec(key, "key"))?;
            let modifiers = match present(object, "modifiers") {
                None => Vec::new(),
                Some(Value::Array(modifiers)) if modifiers.len() <= 5 => modifiers
                    .iter()
                    .map(modifier_spec)
                    .collect::<Result<Vec<_>, _>>()?,
                Some(_) => {
                    return Err(invalid_argument(
                        "modifiers",
                        "list of modifier names",
                        "shift|ctrl|alt|super|altgr",
                    ));
                }
            };
            Ok(InputOp::Key {
                key,
                action: press_action(object, "tap")?,
                modifiers,
            })
        }
        "comp.input.release_all" => {
            args_object(args, &empty, &[])?;
            Ok(InputOp::ReleaseAll)
        }
        _ => Err(invalid_argument(
            "verb",
            "input verb",
            "comp.input.pointer.move|pointer.button|pointer.scroll|key|release_all",
        )),
    }
}

fn delay_arg(value: Option<&Value>, name: &'static str) -> Result<Option<Duration>, ControlReply> {
    match value {
        None => Ok(None),
        Some(value) => value
            .as_u64()
            .filter(|ms| *ms <= LONG_VERB_MAX.as_millis() as u64)
            .map(|ms| Some(Duration::from_millis(ms)))
            .ok_or_else(|| invalid_argument(name, "unsigned integer", "0..=60000")),
    }
}

#[cfg(test)]
mod region_argument_tests {
    use super::*;
    #[test]
    fn region_arguments_are_strict_and_leave_reply_margin() {
        for args in [
            json!({"timeout_ms":0}),
            json!({"timeout_ms":55_001}),
            json!({"timeout_ms":2.5}),
            json!({"output":""}),
            json!({"region":{}}),
        ] {
            assert!(parse_region_select(&args).is_err(), "{args}");
        }
        let op = parse_region_select(&json!({"output":"Output-1","timeout_ms":55_000})).unwrap();
        assert_eq!(op.budget(), Duration::from_secs(59));
        assert!(op.budget() < LONG_VERB_MAX);
        let reply_budget =
            Duration::from_secs(55) + crate::protocol::region_selection::REGION_CLEANUP_BUDGET;
        assert_eq!(reply_budget, Duration::from_secs(58));
        assert_eq!(op.budget(), reply_budget + Duration::from_secs(1));
        assert_eq!(
            op.budget().min(LONG_VERB_MAX) + LONG_VERB_SLACK,
            reply_budget + Duration::from_secs(2)
        );
        assert!(
            matches!(parse_region_select(&json!({})).unwrap(),LongOp::RegionSelect {output:None,timeout} if timeout==Duration::from_secs(30))
        );
    }
}

fn parse_region_select(args: &Value) -> Result<LongOp, ControlReply> {
    let empty = serde_json::Map::new();
    let object = args_object(args, &empty, &["output", "timeout_ms"])?;
    let output = match object.get("output") {
        None => None,
        Some(Value::String(name)) if !name.is_empty() => Some(name.clone()),
        _ => {
            return Err(invalid_argument(
                "output",
                "non-empty string",
                "output name",
            ));
        }
    };
    let timeout = match object.get("timeout_ms") {
        None => 30_000,
        Some(value) => value
            .as_u64()
            .filter(|ms| (1..=55_000).contains(ms))
            .ok_or_else(|| {
                invalid_argument("timeout_ms", "integer 1..55000", "selection deadline")
            })?,
    };
    Ok(LongOp::RegionSelect {
        output,
        timeout: Duration::from_millis(timeout),
    })
}

/// `comp.input.sequence {steps:[{verb, args?, delay_ms?}], interval_ms?}`.
/// `delay_ms` (default `interval_ms`, default 0) runs before its step; the
/// delays together are capped at 60 s.
fn parse_sequence(args: &Value) -> Result<LongOp, ControlReply> {
    let empty = serde_json::Map::new();
    const ALLOWED: &[&str] = &["steps", "interval_ms"];
    let object = args_object(args, &empty, ALLOWED)?;
    let interval = delay_arg(present(object, "interval_ms"), "interval_ms")?.unwrap_or_default();
    let steps = match present(object, "steps") {
        Some(Value::Array(steps)) if !steps.is_empty() && steps.len() <= SEQUENCE_MAX_STEPS => {
            steps
        }
        _ => return Err(invalid_argument("steps", "list", "1..=256 steps")),
    };
    let mut parsed = Vec::with_capacity(steps.len());
    let mut total = Duration::ZERO;
    let mut events = 0_usize;
    for (index, step) in steps.iter().enumerate() {
        const STEP: &[&str] = &["verb", "args", "delay_ms"];
        let step = match step {
            Value::Object(step) => step,
            _ => {
                return Err(invalid_argument(
                    "steps",
                    "list of objects",
                    "{verb, args?, delay_ms?}",
                ));
            }
        };
        if let Some(field) = step.keys().find(|field| !STEP.contains(&field.as_str())) {
            return Err(ControlReply::InvalidArgs {
                field: format!("steps[{index}].{field}"),
                allowed: STEP,
            });
        }
        let verb = present(step, "verb")
            .and_then(Value::as_str)
            .and_then(input_verb)
            .ok_or_else(|| {
                invalid_argument(
                    "steps.verb",
                    "input verb",
                    "comp.input.pointer.move|pointer.button|pointer.scroll|key|release_all",
                )
            })?;
        let op =
            parse_input_op(verb, step.get("args").unwrap_or(&Value::Null)).map_err(|reply| {
                match reply {
                    ControlReply::Validation(SetValidationError::InvalidValue {
                        path,
                        expected,
                        range,
                    }) => ControlReply::Validation(SetValidationError::InvalidValue {
                        path: format!("steps[{index}].args.{path}"),
                        expected,
                        range,
                    }),
                    ControlReply::InvalidArgs { field, allowed } => ControlReply::InvalidArgs {
                        field: format!("steps[{index}].args.{field}"),
                        allowed,
                    },
                    other => other,
                }
            })?;
        events += op.event_bound();
        if events > MAX_EVENTS_PER_VERB {
            return Err(invalid_argument(
                "steps",
                "injected events",
                "at most 4096 injected events per verb (MAX_EVENTS_PER_VERB)",
            ));
        }
        let delay = delay_arg(present(step, "delay_ms"), "steps.delay_ms")?.unwrap_or(interval);
        total += delay;
        if total > LONG_VERB_MAX {
            return Err(invalid_argument(
                "steps",
                "total delay",
                "the delays together are at most 60000 ms",
            ));
        }
        parsed.push(SequenceStep { verb, op, delay });
    }
    Ok(LongOp::Sequence(parsed))
}

/// `comp.window.stats {id, generation | source, registration?, samples?}`
/// and `comp.window.stats.reset {id, generation | source, registration? |
/// nothing}`. `{id, generation}` and `{source}` are mutually exclusive.
fn parse_stats_op(verb: &str, args: &Value) -> Result<WindowOp, ControlReply> {
    const STATS_ARGS: &[&str] = &["id", "generation", "source", "registration", "samples"];
    const RESET_ARGS: &[&str] = &["id", "generation", "source", "registration"];
    let reset = verb == "comp.window.stats.reset";
    let empty = serde_json::Map::new();
    let object = match args {
        Value::Null => &empty,
        Value::Object(object) => object,
        _ => {
            return Err(invalid_argument(
                "args",
                "JSON object",
                "{id, generation} or {source}",
            ));
        }
    };
    let allowed = if reset { RESET_ARGS } else { STATS_ARGS };
    if let Some(field) = object
        .keys()
        .find(|field| !allowed.contains(&field.as_str()))
    {
        return Err(ControlReply::InvalidArgs {
            field: field.clone(),
            allowed,
        });
    }
    let id = window_arg(object, "id")?;
    let generation = window_arg(object, "generation")?;
    let registration = window_arg(object, "registration")?;
    let source = match object.get("source") {
        None | Some(Value::Null) => None,
        Some(Value::String(source))
            if crate::content_source::ContentSourceId::new(source.as_str()).is_ok() =>
        {
            Some(source.clone())
        }
        Some(_) => {
            return Err(invalid_argument("source", "string", "[a-z0-9_-]{1,64}"));
        }
    };
    let window = match (id, generation) {
        (Some(id), Some(generation)) => Some((id, generation)),
        (None, None) => None,
        (Some(_), None) => {
            return Err(invalid_argument(
                "generation",
                "unsigned integer",
                "required with id (read windows.s<id>.generation)",
            ));
        }
        (None, Some(_)) => {
            return Err(invalid_argument(
                "id",
                "unsigned integer",
                "required with generation",
            ));
        }
    };
    let target = match (window, source) {
        (Some(_), Some(_)) => {
            return Err(invalid_argument(
                "source",
                "absent when id is given",
                "{id, generation} and {source} are mutually exclusive",
            ));
        }
        (Some((id, generation)), None) => {
            if registration.is_some() {
                return Err(invalid_argument(
                    "registration",
                    "absent when id is given",
                    "only with source (read sources.<id>.registration)",
                ));
            }
            Some(StatsTarget::Window { id, generation })
        }
        (None, Some(id)) => Some(StatsTarget::Source { id, registration }),
        (None, None) => {
            if registration.is_some() {
                return Err(invalid_argument(
                    "source",
                    "string",
                    "required with registration",
                ));
            }
            None
        }
    };
    if reset {
        return Ok(WindowOp::StatsReset { target });
    }
    let Some(target) = target else {
        return Err(invalid_argument(
            "id",
            "unsigned integer",
            "required (with generation), or give source",
        ));
    };
    let samples = match window_arg(object, "samples")? {
        None => STATS_RING,
        Some(samples) if samples <= STATS_RING as u64 => samples as usize,
        Some(_) => {
            return Err(invalid_argument("samples", "unsigned integer", "0..=512"));
        }
    };
    Ok(WindowOp::Stats { target, samples })
}

/// The tree path a read verb can reach; `None` for the whole tree.
///
/// A `list` prefix and a scope relate to paths the same way: at segment
/// boundaries (`PropPath::starts_with`, `ReadScopes::wants`). A mid-segment
/// prefix such as `"windows.s"` therefore lists nothing and scopes nothing,
/// consistently, so the raw prefix is the scope.
fn read_scope(verb: &str, args: &Value) -> Option<String> {
    // Explicit per verb: a verb added to `needs_snapshot` later must say
    // what its scope is, rather than inherit "path" and be mis-scoped by a
    // same-named argument that means something else.
    let key = match verb {
        "comp.info" => return Some("info".to_string()),
        "comp.props.list" => "prefix",
        "comp.props.get" | "comp.props.describe" => "path",
        _ => return None,
    };
    args.get(key).and_then(Value::as_str).map(str::to_string)
}

fn invalid_set_shape(path: Option<&str>) -> (u8, Arc<str>) {
    (
        10,
        Arc::from(
            json!({
                "error": "invalid_value",
                "path": path,
                "expected": "JSON property value",
                "range": "descriptor",
            })
            .to_string(),
        ),
    )
}

/// Admit a long verb under the long pool and reply when it resolves.
#[allow(clippy::too_many_arguments)]
fn spawn_long_verb(
    ingress: &PortIngress,
    responders: &mut JoinSet<()>,
    long_permits: &Arc<Semaphore>,
    reply_sender: &tokio_mpsc::Sender<PendingReply>,
    reply_timeouts: &Arc<AtomicU64>,
    command: cosmix_client::IncomingCommand,
    op: LongOp,
) {
    let permit = match Arc::clone(long_permits).try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            queue_reply(
                reply_sender,
                reply_timeouts,
                PendingReply::new(command, error("busy")),
            );
            return;
        }
    };
    let admission = match ingress.request_long(op) {
        Ok(admission) => admission,
        Err(()) => {
            queue_reply(
                reply_sender,
                reply_timeouts,
                PendingReply::new(command, error("busy")),
            );
            return;
        }
    };
    let reply_sender = reply_sender.clone();
    let reply_timeouts = Arc::clone(reply_timeouts);
    responders.spawn(async move {
        let _permit = permit;
        let reply = admission
            .receive()
            .await
            .unwrap_or(ControlReply::Busy)
            .into_wire();
        queue_reply(
            &reply_sender,
            &reply_timeouts,
            PendingReply::new(command, reply),
        );
    });
}

fn spawn_control_responder(
    responders: &mut JoinSet<()>,
    reply_sender: &tokio_mpsc::Sender<PendingReply>,
    reply_timeouts: &Arc<AtomicU64>,
    command: cosmix_client::IncomingCommand,
    admission: ControlAdmission,
    permit: tokio::sync::OwnedSemaphorePermit,
) {
    let reply_sender = reply_sender.clone();
    let reply_timeouts = Arc::clone(reply_timeouts);
    responders.spawn(async move {
        let _permit = permit;
        let reply = admission
            .receive()
            .await
            .unwrap_or(ControlReply::Busy)
            .into_wire();
        queue_reply(
            &reply_sender,
            &reply_timeouts,
            PendingReply::new(command, reply),
        );
    });
}

fn queue_reply(
    sender: &tokio_mpsc::Sender<PendingReply>,
    reply_timeouts: &AtomicU64,
    reply: PendingReply,
) {
    if let Err(error) = sender.try_send(reply)
        && matches!(error, tokio_mpsc::error::TrySendError::Full(_))
    {
        reply_timeouts.fetch_add(1, Ordering::AcqRel);
    }
}

async fn reply_loop<C: WorkerClient>(
    client: Arc<C>,
    service: Arc<str>,
    mut replies: tokio_mpsc::Receiver<PendingReply>,
    reply_timeouts: Arc<AtomicU64>,
) {
    while let Some(reply) = replies.recv().await {
        let Some(reply) = enforce_reply_wire_limit(&service, reply) else {
            tracing::debug!(
                service = %service,
                "compositor Bus reply headers exceed the broker WebSocket frame cap"
            );
            continue;
        };
        match tokio::time::timeout(REPLY_SEND_TIMEOUT, client.respond_parts(&reply)).await {
            Err(_) => {
                reply_timeouts.fetch_add(1, Ordering::AcqRel);
                tracing::debug!(
                    command = %reply.command,
                    timeout_ms = REPLY_SEND_TIMEOUT.as_millis(),
                    "compositor Bus reply timed out"
                );
            }
            Ok(Err(error)) => {
                tracing::debug!(%error, command = %reply.command, "compositor Bus reply failed");
            }
            Ok(Ok(())) => {}
        }
    }
}

async fn publisher_loop<C: WorkerClient>(
    client: Arc<C>,
    service: Arc<str>,
    observations: ObservationOutbox,
    observation_notifier: Arc<tokio::sync::Notify>,
    lost_count: Arc<AtomicU64>,
    publish_timeouts: Arc<AtomicU64>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut pending_gap: Option<LossInterval> = None;
    let mut gap_retry_delay = None;
    // The armed deadline outlives a wake that attempts nothing: `sleep_until`
    // it, so elapsed backoff is never discarded by a stale Notify permit.
    let mut gap_retry_deadline: Option<tokio::time::Instant> = None;
    let mut retry_gap_without_data = false;
    let mut connection_states = client.subscribe_state();
    loop {
        let mut lane_empty = false;
        let mut disconnected = false;
        let mut gap_failed_this_pass = false;
        let mut saw_record = false;
        let connection_edge = connection_states.has_changed().unwrap_or(false);
        if connection_edge {
            connection_states.borrow_and_update();
        }

        for _ in 0..observations.capacity {
            let carried = match observations.records.try_recv() {
                Ok(record) => {
                    saw_record = true;
                    record
                }
                Err(crossbeam_channel::TryRecvError::Empty) => {
                    lane_empty = true;
                    break;
                }
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    lane_empty = true;
                    disconnected = true;
                    break;
                }
            };

            if let Some(loss) = carried.preceding_loss {
                merge_pending_gap(&mut pending_gap, loss);
            }
            let record = carried.record;
            let topic_suffix = record.topic_suffix();
            let gap_result = publish_pending_gap_for_topic(
                client.as_ref(),
                &service,
                &lost_count,
                &mut pending_gap,
                topic_suffix,
            )
            .await;
            if gap_result.is_err() {
                publish_timeouts.fetch_add(1, Ordering::AcqRel);
                let (discarded, loss) = discard_publication_backlog(Some(record), &observations);
                lost_count.fetch_add(discarded, Ordering::AcqRel);
                if let Some(loss) = loss {
                    merge_pending_gap(&mut pending_gap, loss);
                }
                arm_gap_retry(&mut gap_retry_delay);
                gap_retry_deadline =
                    gap_retry_delay.map(|delay| tokio::time::Instant::now() + delay);
                gap_failed_this_pass = true;
                break;
            }
            if pending_gap.is_none() {
                gap_retry_delay = None;
                gap_retry_deadline = None;
            }

            let topic = port_observation::topic_name(&service, topic_suffix);
            let message = record.wire();
            if publish_message(client.as_ref(), &topic, &message)
                .await
                .is_ok()
            {
                continue;
            }

            publish_timeouts.fetch_add(1, Ordering::AcqRel);
            let (discarded, loss) = discard_publication_backlog(Some(record), &observations);
            lost_count.fetch_add(discarded, Ordering::AcqRel);
            if let Some(loss) = loss {
                merge_pending_gap(&mut pending_gap, loss);
            }
            arm_gap_retry(&mut gap_retry_delay);
            gap_retry_deadline = gap_retry_delay.map(|delay| tokio::time::Instant::now() + delay);
            gap_failed_this_pass = true;
            break;
        }

        lane_empty |= observations.records.is_empty();
        if lane_empty
            && pending_gap.is_some()
            && !gap_failed_this_pass
            && (saw_record || connection_edge || retry_gap_without_data)
            && publish_pending_gap(client.as_ref(), &service, &lost_count, &mut pending_gap)
                .await
                .is_err()
        {
            publish_timeouts.fetch_add(1, Ordering::AcqRel);
            let (discarded, loss) = discard_publication_backlog(None, &observations);
            lost_count.fetch_add(discarded, Ordering::AcqRel);
            if let Some(loss) = loss {
                merge_pending_gap(&mut pending_gap, loss);
            }
            arm_gap_retry(&mut gap_retry_delay);
            gap_retry_deadline = gap_retry_delay.map(|delay| tokio::time::Instant::now() + delay);
        }
        if pending_gap.is_none() {
            gap_retry_delay = None;
            gap_retry_deadline = None;
        }

        if disconnected {
            if !*shutdown.borrow() {
                // Fail fast, but say so where an operator can see it: a
                // silent task death here would leave the worker running while
                // publication stops and the lane overflows.
                tracing::error!(
                    "observation producer disconnected before port shutdown; publisher task aborting"
                );
            }
            assert!(
                *shutdown.borrow(),
                "observation producer disconnected before port shutdown"
            );
            // WaylandRuntime signals port shutdown before its protocol state
            // drops the sole producer. A pending gap may remain only here,
            // under the accepted bounded-shutdown posture above.
            break;
        }
        retry_gap_without_data =
            match wait_for_publisher_wake(&observation_notifier, &mut shutdown, gap_retry_deadline)
                .await
            {
                PublisherWake::Notified => false,
                PublisherWake::RetryTimer => true,
                PublisherWake::Shutdown => break,
            };
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PublisherWake {
    Notified,
    RetryTimer,
    Shutdown,
}

fn arm_gap_retry(delay: &mut Option<Duration>) {
    *delay = Some(
        delay
            .map(|current| current.saturating_mul(2).min(GAP_RETRY_MAX))
            .unwrap_or(GAP_RETRY_INITIAL),
    );
}

async fn wait_for_publisher_wake(
    notifier: &tokio::sync::Notify,
    shutdown: &mut watch::Receiver<bool>,
    retry_deadline: Option<tokio::time::Instant>,
) -> PublisherWake {
    if let Some(deadline) = retry_deadline {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_ok() && !*shutdown.borrow() {
                    PublisherWake::Notified
                } else {
                    PublisherWake::Shutdown
                }
            }
            _ = notifier.notified() => PublisherWake::Notified,
            _ = tokio::time::sleep_until(deadline) => PublisherWake::RetryTimer,
        }
    } else {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_ok() && !*shutdown.borrow() {
                    PublisherWake::Notified
                } else {
                    PublisherWake::Shutdown
                }
            }
            _ = notifier.notified() => PublisherWake::Notified,
        }
    }
}

fn merge_pending_gap(pending: &mut Option<LossInterval>, loss: LossInterval) {
    if let Some(pending) = pending {
        pending.merge(loss);
    } else {
        *pending = Some(loss);
    }
}

async fn publish_pending_gap_for_topic<C: WorkerClient>(
    client: &C,
    service: &str,
    lost_count: &AtomicU64,
    pending: &mut Option<LossInterval>,
    topic_suffix: &str,
) -> Result<(), ()> {
    let Some(mut gap) = *pending else {
        return Ok(());
    };
    if !gap.topics.contains(topic_suffix) {
        return Ok(());
    }
    let topic = port_observation::topic_name(service, topic_suffix);
    let message = gap_message(topic_suffix, gap, lost_count.load(Ordering::Acquire));
    publish_message(client, &topic, &message).await?;
    gap.topics.remove(topic_suffix);
    *pending = (!gap.topics.is_empty()).then_some(gap);
    Ok(())
}

async fn publish_pending_gap<C: WorkerClient>(
    client: &C,
    service: &str,
    lost_count: &AtomicU64,
    pending: &mut Option<LossInterval>,
) -> Result<(), ()> {
    let Some(gap) = *pending else {
        return Ok(());
    };
    let mut remaining = gap;
    for suffix in gap.topics.iter() {
        let topic = port_observation::topic_name(service, suffix);
        let message = gap_message(suffix, gap, lost_count.load(Ordering::Acquire));
        if publish_message(client, &topic, &message).await.is_err() {
            *pending = Some(remaining);
            return Err(());
        }
        remaining.topics.remove(suffix);
    }
    *pending = None;
    Ok(())
}

fn discard_publication_backlog(
    failed: Option<ObservationRecord>,
    observations: &ObservationOutbox,
) -> (u64, Option<LossInterval>) {
    let mut discarded = 0_u64;
    let mut loss: Option<LossInterval> = None;
    let mut absorb = |interval: LossInterval| {
        if let Some(current) = loss.as_mut() {
            current.merge(interval);
        } else {
            loss = Some(interval);
        }
    };
    if let Some(failed) = failed {
        discarded = 1;
        absorb(LossInterval::from_record(&failed, LossCause::PublisherLoss));
    }
    for _ in 0..observations.capacity {
        let Ok(record) = observations.records.try_recv() else {
            break;
        };
        if let Some(preceding) = record.preceding_loss {
            absorb(preceding);
        }
        discarded = discarded.saturating_add(1);
        absorb(LossInterval::from_record(
            &record.record,
            LossCause::PublisherLoss,
        ));
    }
    (discarded, loss)
}

async fn publish_message<C: WorkerClient>(
    client: &C,
    topic: &str,
    message: &BusMessage,
) -> Result<(), ()> {
    let mut headers = BTreeMap::new();
    headers.insert("name".to_string(), topic.to_string());
    headers.insert("retain".to_string(), "false".to_string());
    let wire = message.to_wire();
    match tokio::time::timeout(PUBLISH_TIMEOUT, client.publish(&headers, &wire)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => {
            tracing::debug!(%error, topic, "compositor Bus topic publication failed");
            Err(())
        }
        Err(_) => {
            tracing::debug!(
                topic,
                timeout_ms = PUBLISH_TIMEOUT.as_millis(),
                "compositor Bus topic publication timed out"
            );
            Err(())
        }
    }
}

fn gap_message(topic_suffix: &str, gap: LossInterval, lost_count: u64) -> BusMessage {
    let mut message = BusMessage::new()
        .with_header("command", topic_suffix)
        .with_header("event_seq", &gap.last_lost_seq.to_string());
    message.body = json!({
        "gap": true,
        "lost_count": lost_count,
        "cause": gap.cause.as_str(),
    })
    .to_string();
    message
}

/// Exact byte count produced by `NodedClient::respond_parts` for this reply.
/// The body stays borrowed: only the small canonical header block is assembled.
fn reply_wire_bytes(service: &str, reply: &PendingReply) -> usize {
    let mut message = BusMessage::new()
        .with_header("command", &reply.command)
        .with_header("from", service)
        .with_header("to", &reply.from)
        .with_header("type", "response")
        .with_header("rc", &reply.rc.to_string());
    if let Some(id) = reply.id.as_deref() {
        message = message.with_header("id", id);
    }
    let header_and_framing = message.to_wire().len();
    header_and_framing
        .checked_add(reply.body.len())
        .and_then(|bytes| {
            bytes.checked_add(usize::from(
                !reply.body.is_empty() && !reply.body.ends_with('\n'),
            ))
        })
        .unwrap_or(usize::MAX)
}

fn enforce_reply_wire_limit(service: &str, mut reply: PendingReply) -> Option<PendingReply> {
    if reply_wire_bytes(service, &reply) > MAX_REPLY_WIRE_BYTES {
        let (rc, body) = too_large(MAX_REPLY_BODY_BYTES);
        (reply.rc, reply.body) = with_error_code(rc, body);
    }
    (reply_wire_bytes(service, &reply) <= MAX_REPLY_WIRE_BYTES).then_some(reply)
}

fn random_instance_id() -> Result<String, String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|error| format!("failed to seed compositor Bus instance id: {error}"))?;
    let mut id = String::with_capacity(32);
    for byte in bytes {
        write!(&mut id, "{byte:02x}").map_err(|error| error.to_string())?;
    }
    Ok(id)
}

pub(crate) fn validate_service_name(name: &str) -> Result<(), String> {
    let valid = (2..=31).contains(&name.len())
        && name
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
    if valid {
        Ok(())
    } else {
        Err(format!(
            "invalid Bus service name '{name}': expected ^[a-z][a-z0-9-]{{1,30}}$"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::BTreeMap,
        future,
        sync::atomic::{AtomicBool, AtomicUsize},
    };

    /// The verb-to-scope mapping is the only production path into the
    /// scoped snapshot (round 1 of the 0.58.0 integration review): pin it,
    /// and pin that a list prefix and its scope agree at segment boundaries.
    #[test]
    fn read_scope_names_each_snapshot_verbs_subtree() {
        let list = |prefix: &str| read_scope("comp.props.list", &json!({ "prefix": prefix }));
        assert_eq!(list("windows"), Some("windows".to_string()));
        assert_eq!(list("windows.s3"), Some("windows.s3".to_string()));
        assert_eq!(list("windows.s"), Some("windows.s".to_string()));
        assert_eq!(
            read_scope("comp.props.list", &json!({})),
            None,
            "no prefix: whole tree"
        );
        assert_eq!(
            read_scope("comp.props.get", &json!({ "path": "windows.s3.visible" })),
            Some("windows.s3.visible".to_string())
        );
        assert_eq!(
            read_scope("comp.props.describe", &json!({ "path": "windows.s3" })),
            Some("windows.s3".to_string())
        );
        assert_eq!(
            read_scope("comp.info", &json!({})),
            Some("info".to_string())
        );
        assert_eq!(read_scope("comp.windows.list", &json!({})), None);
        // A scope reaches its own subtree and no sibling; a mid-segment
        // prefix scopes nothing, exactly as the list itself matches nothing.
        let mut scopes = crate::protocol::port_snapshot::ReadScopes::Paths(Vec::new());
        scopes.add(list("windows.s3").as_deref());
        assert!(scopes.wants("windows.s3.presentation"));
        assert!(!scopes.wants("windows.s30.presentation"));
        let mut partial = crate::protocol::port_snapshot::ReadScopes::Paths(Vec::new());
        partial.add(list("windows.s").as_deref());
        assert!(!partial.wants("windows.s3.presentation"));
    }

    type PublishedMessages = Arc<Mutex<Vec<(BTreeMap<String, String>, String)>>>;

    struct FakeClient {
        incoming: Mutex<Option<tokio_mpsc::UnboundedReceiver<cosmix_client::IncomingCommand>>>,
        states: watch::Sender<ConnState>,
        hang_replies: bool,
        responses_started: Arc<AtomicUsize>,
        publish_mode: Arc<AtomicU8>,
        publish_attempts: Arc<AtomicUsize>,
        reject_publish_attempt: Arc<AtomicUsize>,
        publications: PublishedMessages,
        deregister_hangs: Arc<AtomicBool>,
        deregistered: Arc<AtomicUsize>,
        closed: Arc<AtomicUsize>,
    }

    impl FakeClient {
        fn new(
            initial_state: ConnState,
            hang_replies: bool,
        ) -> (
            Self,
            tokio_mpsc::UnboundedSender<cosmix_client::IncomingCommand>,
            watch::Sender<ConnState>,
        ) {
            let (commands, incoming) = tokio_mpsc::unbounded_channel();
            let (states, _) = watch::channel(initial_state);
            (
                Self {
                    incoming: Mutex::new(Some(incoming)),
                    states: states.clone(),
                    hang_replies,
                    responses_started: Arc::new(AtomicUsize::new(0)),
                    publish_mode: Arc::new(AtomicU8::new(0)),
                    publish_attempts: Arc::new(AtomicUsize::new(0)),
                    reject_publish_attempt: Arc::new(AtomicUsize::new(usize::MAX)),
                    publications: Arc::new(Mutex::new(Vec::new())),
                    deregister_hangs: Arc::new(AtomicBool::new(false)),
                    deregistered: Arc::new(AtomicUsize::new(0)),
                    closed: Arc::new(AtomicUsize::new(0)),
                },
                commands,
                states,
            )
        }
    }

    impl WorkerClient for FakeClient {
        fn incoming(
            &self,
        ) -> Option<tokio_mpsc::UnboundedReceiver<cosmix_client::IncomingCommand>> {
            self.incoming
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take()
        }

        fn state(&self) -> ConnState {
            *self.states.borrow()
        }

        fn subscribe_state(&self) -> watch::Receiver<ConnState> {
            self.states.subscribe()
        }

        fn respond_parts<'a>(
            &'a self,
            _reply: &'a PendingReply,
        ) -> WorkerFuture<'a, Result<(), String>> {
            self.responses_started.fetch_add(1, Ordering::AcqRel);
            if self.hang_replies {
                Box::pin(future::pending())
            } else {
                Box::pin(future::ready(Ok(())))
            }
        }

        fn publish<'a>(
            &'a self,
            headers: &'a BTreeMap<String, String>,
            wire: &'a str,
        ) -> WorkerFuture<'a, Result<(), String>> {
            let attempt = self.publish_attempts.fetch_add(1, Ordering::AcqRel) + 1;
            if attempt == self.reject_publish_attempt.load(Ordering::Acquire) {
                return Box::pin(future::ready(Err("publication rejected".into())));
            }
            match self.publish_mode.load(Ordering::Acquire) {
                1 => Box::pin(future::ready(Err("publication rejected".into()))),
                2 => Box::pin(future::pending()),
                _ => {
                    self.publications
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .push((headers.clone(), wire.to_string()));
                    Box::pin(future::ready(Ok(())))
                }
            }
        }

        fn deregister(&self) -> WorkerFuture<'_, Result<(), String>> {
            self.deregistered.fetch_add(1, Ordering::AcqRel);
            if self.deregister_hangs.load(Ordering::Acquire) {
                Box::pin(future::pending())
            } else {
                Box::pin(future::ready(Ok(())))
            }
        }

        fn close(&self) -> WorkerFuture<'_, ()> {
            self.closed.fetch_add(1, Ordering::AcqRel);
            Box::pin(future::ready(()))
        }

        fn subscribe_topic<'a>(&'a self, _topic: &'a str) -> WorkerFuture<'a, Result<(), String>> {
            Box::pin(future::ready(Ok(())))
        }
    }

    fn test_ingress() -> (PortIngress, channel::Channel<PortCommand>, Arc<AtomicUsize>) {
        let queue_depth = Arc::new(AtomicUsize::new(0));
        let (sender, source) = channel::sync_channel(PORT_QUEUE_CAPACITY);
        (
            PortIngress {
                sender,
                queue_depth: Arc::clone(&queue_depth),
                control_order: Arc::new(AtomicU64::new(0)),
                pending_idle_order: Arc::new(AtomicU64::new(0)),
                pending_active_order: Arc::new(AtomicU64::new(0)),
            },
            source,
            queue_depth,
        )
    }

    fn test_observation_args() -> (Arc<AtomicU64>, ObservationOutbox, Arc<AtomicU64>) {
        let lost = Arc::new(AtomicU64::new(0));
        let (_producer, receiver) = port_observation::outbox(Arc::clone(&lost));
        (Arc::new(AtomicU64::new(0)), receiver, lost)
    }

    fn command(command: &str, id: usize) -> cosmix_client::IncomingCommand {
        cosmix_client::IncomingCommand {
            from: "test-caller".into(),
            command: command.into(),
            id: Some(id.to_string()),
            args: Value::Null,
            body: String::new(),
            headers: BTreeMap::new(),
        }
    }

    async fn wait_for_broker(broker: &AtomicU8, expected: u8) {
        tokio::time::timeout(Duration::from_secs(1), async {
            while broker.load(Ordering::Acquire) != expected {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("worker publishes broker edge");
    }

    async fn wait_for_counter(counter: &AtomicUsize, expected: usize, message: &'static str) {
        tokio::time::timeout(Duration::from_secs(1), async {
            while counter.load(Ordering::Acquire) != expected {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect(message);
    }

    async fn next_port_command(source: &channel::Channel<PortCommand>) -> PortCommand {
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                match source.try_recv() {
                    Ok(command) => return command,
                    Err(mpsc::TryRecvError::Empty) => tokio::task::yield_now().await,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        panic!("port source disconnected")
                    }
                }
            }
        })
        .await
        .expect("worker admits command")
    }

    #[tokio::test(start_paused = true)]
    async fn worker_refusal_stays_retrying_without_panicking() {
        let (_shutdown_tx, mut shutdown) = watch::channel(false);
        let broker = AtomicU8::new(BROKER_CONNECTED);
        let attempts = AtomicUsize::new(0);
        let (_sender, protocol_source) = channel::sync_channel::<PortCommand>(1);
        let result = connect_loop(
            &mut shutdown,
            &broker,
            || {
                attempts.fetch_add(1, Ordering::Relaxed);
                std::future::ready(Err::<(), _>(ConnectAttemptError::Retry(
                    "connection refused".into(),
                )))
            },
            Duration::from_millis(1),
        );
        tokio::pin!(result);
        tokio::select! {
            _ = &mut result => panic!("refused connector must keep retrying"),
            _ = async {
                tokio::task::yield_now().await;
                tokio::time::advance(Duration::from_millis(250)).await;
                tokio::task::yield_now().await;
            } => {}
        }
        assert_eq!(broker.load(Ordering::Acquire), BROKER_RETRYING);
        assert!(attempts.load(Ordering::Relaxed) >= 1);
        assert!(matches!(
            protocol_source.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn complete_worker_loop_tracks_state_edges_without_polling() {
        let (ingress, _source, _) = test_ingress();
        let broker = Arc::new(AtomicU8::new(BROKER_RETRYING));
        let reply_timeouts = Arc::new(AtomicU64::new(0));
        let (publish_timeouts, observations, lost_count) = test_observation_args();
        let (shutdown_tx, shutdown) = watch::channel(false);
        let (client, _commands, states) = FakeClient::new(ConnState::Connected, false);
        let mut client = Some(client);
        assert_eq!(broker.load(Ordering::Acquire), BROKER_RETRYING);
        let worker = tokio::spawn(worker_loop(
            "comp-nested".into(),
            ingress,
            Arc::clone(&broker),
            reply_timeouts,
            publish_timeouts,
            observations,
            Arc::new(tokio::sync::Notify::new()),
            lost_count,
            shutdown,
            move || future::ready(Ok(client.take().expect("one connection attempt"))),
        ));

        wait_for_broker(&broker, BROKER_CONNECTED).await;
        states.send_replace(ConnState::Disconnected);
        wait_for_broker(&broker, BROKER_RETRYING).await;
        states.send_replace(ConnState::Connected);
        wait_for_broker(&broker, BROKER_CONNECTED).await;

        shutdown_tx.send_replace(true);
        worker.await.expect("worker exits cleanly");
    }

    #[tokio::test]
    async fn complete_worker_loop_terminates_on_fatal_reconnect_collision() {
        let (ingress, _source, _) = test_ingress();
        let broker = Arc::new(AtomicU8::new(BROKER_RETRYING));
        let (_shutdown_tx, shutdown) = watch::channel(false);
        let (client, _commands, states) = FakeClient::new(ConnState::Connected, false);
        let mut client = Some(client);
        let (publish_timeouts, observations, lost_count) = test_observation_args();
        let worker = tokio::spawn(worker_loop(
            "comp-nested".into(),
            ingress,
            Arc::clone(&broker),
            Arc::new(AtomicU64::new(0)),
            publish_timeouts,
            observations,
            Arc::new(tokio::sync::Notify::new()),
            lost_count,
            shutdown,
            move || future::ready(Ok(client.take().expect("one connection attempt"))),
        ));

        wait_for_broker(&broker, BROKER_CONNECTED).await;
        states.send_replace(ConnState::Fatal);
        tokio::time::timeout(Duration::from_secs(1), worker)
            .await
            .expect("fatal reconnect collision terminates worker")
            .expect("worker exits cleanly");
        assert_eq!(broker.load(Ordering::Acquire), BROKER_RETRYING);
    }

    #[tokio::test]
    async fn complete_worker_loop_terminates_on_registration_rejection_without_renaming() {
        let (ingress, _source, _) = test_ingress();
        let broker = Arc::new(AtomicU8::new(BROKER_CONNECTED));
        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts_for_connector = Arc::clone(&attempts);
        let (_shutdown_tx, shutdown) = watch::channel(false);
        let (publish_timeouts, observations, lost_count) = test_observation_args();
        worker_loop(
            "comp-nested".into(),
            ingress,
            Arc::clone(&broker),
            Arc::new(AtomicU64::new(0)),
            publish_timeouts,
            observations,
            Arc::new(tokio::sync::Notify::new()),
            lost_count,
            shutdown,
            move || {
                attempts_for_connector.fetch_add(1, Ordering::Relaxed);
                future::ready(Err::<FakeClient, _>(
                    ConnectAttemptError::RegistrationRejected {
                        service: "comp-nested".into(),
                        rc: 10,
                        message: "registration refused".into(),
                    },
                ))
            },
        )
        .await;
        assert_eq!(attempts.load(Ordering::Relaxed), 1);
        assert_eq!(broker.load(Ordering::Acquire), BROKER_RETRYING);
    }

    #[test]
    fn service_name_validation_matches_abp_grammar() {
        for valid in ["comp", "comp-nested", "a0"] {
            assert!(validate_service_name(valid).is_ok(), "{valid}");
        }
        for invalid in ["c", "Comp", "comp_nested", "-comp", "comp-é"] {
            assert!(validate_service_name(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn reply_headroom_covers_maximal_headers_and_exact_body_limit_fits() {
        let maximal_service = format!("a{}", "z".repeat(30));
        assert!(validate_service_name(&maximal_service).is_ok());
        let reply = PendingReply {
            from: maximal_service.clone(),
            command: "comp.props.describe".into(),
            id: Some(format!("noded-{}", u64::MAX)),
            rc: 0,
            body: Arc::from("x".repeat(MAX_REPLY_BODY_BYTES)),
        };
        let wire_bytes = reply_wire_bytes(&maximal_service, &reply);
        let header_and_framing = wire_bytes - reply.body.len();
        assert!(
            header_and_framing <= port_snapshot::REPLY_WIRE_HEADROOM_BYTES,
            "{header_and_framing} header/framing bytes exceed the documented reserve"
        );
        assert!(wire_bytes <= MAX_REPLY_WIRE_BYTES);

        let checked = enforce_reply_wire_limit(&maximal_service, reply)
            .expect("maximal canonical reply headers fit");
        assert_eq!(checked.rc, 0);
        assert_eq!(checked.body.len(), MAX_REPLY_BODY_BYTES);
    }

    #[test]
    fn measured_wire_overflow_becomes_too_large() {
        let reply = PendingReply {
            from: "requester".into(),
            command: "comp.props.get".into(),
            id: Some("noded-1".into()),
            rc: 0,
            body: Arc::from("x".repeat(MAX_REPLY_WIRE_BYTES)),
        };
        assert!(reply_wire_bytes("comp-nested", &reply) > MAX_REPLY_WIRE_BYTES);

        let checked =
            enforce_reply_wire_limit("comp-nested", reply).expect("too_large response fits");
        assert_eq!(checked.rc, 10);
        assert_eq!(
            serde_json::from_str::<Value>(&checked.body).expect("too_large JSON"),
            serde_json::json!({
                "error": "too_large",
                "error_code": "too_large",
                "limit_bytes": MAX_REPLY_BODY_BYTES,
                "hint": "read a subtree",
            })
        );
        assert!(reply_wire_bytes("comp-nested", &checked) <= MAX_REPLY_WIRE_BYTES);
    }

    #[tokio::test]
    async fn ping_ignores_malformed_body_while_property_reads_reject_it() {
        let (ingress, source, _) = test_ingress();
        let mut responders = JoinSet::new();
        let responder_permits = Arc::new(Semaphore::new(PORT_QUEUE_CAPACITY));
        let (reply_sender, mut replies) = tokio_mpsc::channel(2);
        let reply_timeouts = Arc::new(AtomicU64::new(0));

        let mut ping = command("comp.ping", 1);
        ping.body = "{".into();
        handle_incoming(
            &ingress,
            &mut responders,
            &responder_permits,
            &reply_sender,
            &reply_timeouts,
            "comp-nested",
            ping,
        );
        let reply = replies.recv().await.expect("ping reply queued");
        assert_eq!(reply.rc, 0);
        assert_eq!(reply.body.as_ref(), "{\"pong\":true}");

        let mut get = command("comp.props.get", 2);
        get.body = "{".into();
        handle_incoming(
            &ingress,
            &mut responders,
            &responder_permits,
            &reply_sender,
            &reply_timeouts,
            "comp-nested",
            get,
        );
        let reply = replies.recv().await.expect("malformed read reply queued");
        assert_eq!(reply.rc, 10);
        assert_eq!(
            reply.body.as_ref(),
            "{\"error\":\"unknown_path\",\"error_code\":\"unknown_path\"}"
        );
        assert!(matches!(source.try_recv(), Err(mpsc::TryRecvError::Empty)));
    }

    fn local_set_command(id: usize, path: &str, value: Value) -> cosmix_client::IncomingCommand {
        let mut command = command("comp.props.set", id);
        command.args = json!({"path": path, "value": value});
        command.body = command.args.to_string();
        command
            .headers
            .insert("broker_origin".into(), "local".into());
        command
    }

    /// Mesh law (2026-09-15): being on the mesh is the whole authorization.
    /// A mesh-stamped caller with peer/identity headers, a caller with no
    /// origin stamp at all, and one with an unusual name all reach the
    /// calloop exactly like a local one, for writes and pointer watches.
    #[tokio::test]
    async fn mesh_and_unstamped_callers_reach_set_and_pointer_watch() {
        let mut mesh = local_set_command(1, "input.corners.enabled", json!(true));
        mesh.headers.insert("broker_origin".into(), "mesh".into());
        mesh.headers
            .insert("source_peer".into(), "beta.example".into());
        mesh.headers.insert("signed_ident".into(), "opaque".into());
        let mut unstamped = local_set_command(2, "input.corners.enabled", json!(false));
        unstamped.headers.clear();
        let mut odd_caller = local_set_command(3, "input.corners.dwell_ms", json!(250));
        odd_caller.from = "anonymous".into();
        let mut mesh_pointer = command("comp.pointer.watch", 4);
        mesh_pointer
            .headers
            .insert("broker_origin".into(), "mesh".into());
        mesh_pointer
            .headers
            .insert("source_peer".into(), "beta.example".into());

        for command in [mesh, unstamped, odd_caller, mesh_pointer] {
            let (ingress, source, _) = test_ingress();
            let mut responders = JoinSet::new();
            let permits = Arc::new(Semaphore::new(PORT_QUEUE_CAPACITY));
            let (reply_sender, mut replies) = tokio_mpsc::channel(2);
            let reply_timeouts = Arc::new(AtomicU64::new(0));
            let verb = command.command.clone();
            let id = command.id.clone();
            handle_incoming(
                &ingress,
                &mut responders,
                &permits,
                &reply_sender,
                &reply_timeouts,
                "comp-nested",
                command,
            );
            match source.try_recv() {
                Ok(PortCommand::Set(_)) => assert_eq!(verb, "comp.props.set", "{id:?}"),
                Ok(PortCommand::PointerWatch(_)) => {
                    assert_eq!(verb, "comp.pointer.watch", "{id:?}")
                }
                _ => panic!("{verb} {id:?} was not admitted"),
            }
            assert!(
                replies.try_recv().is_err(),
                "{verb} {id:?} got an early refusal"
            );
            responders.abort_all();
        }
    }

    #[test]
    fn window_verb_arguments_parse_with_both_or_neither_target_fields() {
        assert_eq!(
            parse_window_op("comp.window.restore", &Value::Null),
            Ok(WindowOp::Restore { target: None })
        );
        assert_eq!(
            parse_window_op("comp.window.restore", &json!({})),
            Ok(WindowOp::Restore { target: None })
        );
        assert_eq!(
            parse_window_op("comp.window.restore", &json!({"id": 7, "generation": 3})),
            Ok(WindowOp::Restore {
                target: Some((7, 3))
            })
        );
        assert_eq!(
            parse_window_op("comp.window.minimize", &json!({"id": 7, "generation": 3})),
            Ok(WindowOp::Minimize {
                id: 7,
                generation: 3
            })
        );
        for (verb, args, field) in [
            ("comp.window.restore", json!({"id": 7}), "generation"),
            ("comp.window.restore", json!({"generation": 3}), "id"),
            (
                "comp.window.restore",
                json!({"id": "7", "generation": 3}),
                "id",
            ),
            (
                "comp.window.restore",
                json!({"id": 7, "generation": -1}),
                "generation",
            ),
            ("comp.window.restore", json!([7, 3]), "args"),
            ("comp.window.minimize", json!({}), "id"),
            ("comp.window.minimize", json!({"id": 7}), "generation"),
        ] {
            let Err(reply) = parse_window_op(verb, &args) else {
                panic!("{verb} {args} must be refused");
            };
            let (rc, body) = reply.into_wire();
            let body = serde_json::from_str::<Value>(&body).unwrap();
            assert_eq!(rc, 10, "{verb} {args}");
            assert_eq!(body["error"], "invalid_value", "{verb} {args}");
            assert_eq!(body["path"], field, "{verb} {args}");
        }
    }

    #[test]
    fn stats_verb_arguments_name_a_window_or_a_source() {
        let window = StatsTarget::Window {
            id: 7,
            generation: 3,
        };
        assert_eq!(
            parse_stats_op("comp.window.stats", &json!({"id": 7, "generation": 3})),
            Ok(WindowOp::Stats {
                target: window.clone(),
                samples: STATS_RING,
            })
        );
        assert_eq!(
            parse_stats_op(
                "comp.window.stats",
                &json!({"source": "scene", "registration": 2, "samples": 0})
            ),
            Ok(WindowOp::Stats {
                target: StatsTarget::Source {
                    id: "scene".into(),
                    registration: Some(2),
                },
                samples: 0,
            })
        );
        assert_eq!(
            parse_stats_op("comp.window.stats.reset", &Value::Null),
            Ok(WindowOp::StatsReset { target: None })
        );
        assert_eq!(
            parse_stats_op(
                "comp.window.stats.reset",
                &json!({"id": 7, "generation": 3})
            ),
            Ok(WindowOp::StatsReset {
                target: Some(window)
            })
        );
        assert_eq!(
            parse_stats_op("comp.window.stats.reset", &json!({"source": "scene"})),
            Ok(WindowOp::StatsReset {
                target: Some(StatsTarget::Source {
                    id: "scene".into(),
                    registration: None,
                })
            })
        );
        for (verb, args, field) in [
            (
                "comp.window.stats",
                json!({"id": 7, "generation": 3, "source": "scene"}),
                "source",
            ),
            ("comp.window.stats", json!({}), "id"),
            ("comp.window.stats", json!({"id": 7}), "generation"),
            ("comp.window.stats", json!({"source": "Bad.Id"}), "source"),
            ("comp.window.stats", json!({"source": 7}), "source"),
            (
                "comp.window.stats",
                json!({"source": "scene", "samples": 513}),
                "samples",
            ),
            (
                "comp.window.stats",
                json!({"id": 7, "generation": 3, "registration": 1}),
                "registration",
            ),
            (
                "comp.window.stats.reset",
                json!({"registration": 1}),
                "source",
            ),
            ("comp.window.stats.reset", json!({"generation": 3}), "id"),
            ("comp.window.stats.reset", json!([1]), "args"),
        ] {
            let Err(reply) = parse_stats_op(verb, &args) else {
                panic!("{verb} {args} must be refused");
            };
            let (rc, body) = reply.into_wire();
            let body = serde_json::from_str::<Value>(&body).unwrap();
            assert_eq!(rc, 10, "{verb} {args}");
            assert_eq!(body["error"], "invalid_value", "{verb} {args}");
            assert_eq!(body["path"], field, "{verb} {args}");
        }
        let reply = parse_stats_op("comp.window.stats.reset", &json!({"samples": 1}))
            .expect_err("reset takes no samples");
        let body = serde_json::from_str::<Value>(&reply.into_wire().1).unwrap();
        assert_eq!(body["error"], "invalid_args");
        assert_eq!(body["field"], "samples");
    }

    #[test]
    fn window_verb_unknown_fields_are_refused_by_name() {
        for verb in ["comp.window.restore", "comp.window.minimize"] {
            let reply = parse_window_op(verb, &json!({"id": 7, "gen": 3}))
                .expect_err("a typo must not be ignored");
            let (rc, body) = reply.into_wire();
            assert_eq!(rc, 10);
            assert_eq!(
                serde_json::from_str::<Value>(&body).unwrap(),
                json!({"error": "invalid_args", "field": "gen", "allowed": ["id", "generation"]}),
                "{verb}"
            );
        }
    }

    #[test]
    fn step_eight_window_verbs_parse_and_refuse_by_field() {
        assert_eq!(
            parse_window_verb("comp.window.focus", &json!({"id": 7, "generation": 3})),
            Ok(WindowVerb::Op(WindowOp::Focus {
                id: 7,
                generation: 3,
                raise: true
            }))
        );
        assert_eq!(
            parse_window_verb(
                "comp.window.focus",
                &json!({"id": 7, "generation": 3, "raise": false})
            ),
            Ok(WindowVerb::Op(WindowOp::Focus {
                id: 7,
                generation: 3,
                raise: false
            }))
        );
        assert_eq!(
            parse_window_verb("comp.window.raise", &json!({"id": 7, "generation": 3})),
            Ok(WindowVerb::Op(WindowOp::Raise {
                id: 7,
                generation: 3
            }))
        );
        assert_eq!(
            parse_window_verb("comp.window.close", &json!({"id": 7, "generation": 3})),
            Ok(WindowVerb::Op(WindowOp::Close {
                id: 7,
                generation: 3
            }))
        );
        assert_eq!(
            parse_window_verb(
                "comp.window.close",
                &json!({"id": 7, "generation": 3, "force": true})
            ),
            Ok(WindowVerb::Long(LongOp::ForceClose {
                id: 7,
                generation: 3,
                timeout: CLOSE_FORCE_DEFAULT
            }))
        );
        assert_eq!(
            parse_window_verb(
                "comp.window.close",
                &json!({"id": 7, "generation": 3, "force": true, "timeout_ms": 250})
            ),
            Ok(WindowVerb::Long(LongOp::ForceClose {
                id: 7,
                generation: 3,
                timeout: Duration::from_millis(250)
            }))
        );
        assert_eq!(
            parse_window_verb(
                "comp.window.place",
                &json!({"id": 7, "generation": 3, "output": "o_x", "x": 10, "height": 300})
            ),
            Ok(WindowVerb::Op(WindowOp::Place(PlaceSpec {
                id: 7,
                generation: 3,
                output: Some("o_x".into()),
                x: Some(10.0),
                y: None,
                width: None,
                height: Some(300),
            })))
        );
        assert_eq!(
            parse_window_verb(
                "comp.window.wait",
                &json!({"match": {"app_id": "a", "title_contains": "b"}, "until": "presented"})
            ),
            Ok(WindowVerb::Long(LongOp::Wait(WaitSpec {
                window: WindowMatch {
                    app_id: Some("a".into()),
                    title_contains: Some("b".into()),
                    ..WindowMatch::default()
                },
                until: WaitUntil::Presented,
                timeout: WINDOW_WAIT_DEFAULT,
            })))
        );
        assert_eq!(
            parse_window_verb(
                "comp.window.wait",
                &json!({
                    "match": {"id": 7},
                    "until": "size",
                    "width": 500,
                    "height": 300,
                    "timeout_ms": 60_000,
                })
            ),
            Ok(WindowVerb::Long(LongOp::Wait(WaitSpec {
                window: WindowMatch {
                    id: Some(7),
                    ..WindowMatch::default()
                },
                until: WaitUntil::Size {
                    width: 500,
                    height: 300
                },
                timeout: LONG_VERB_MAX,
            })))
        );

        for (verb, args, path) in [
            ("comp.window.focus", json!({"id": 7}), "generation"),
            (
                "comp.window.focus",
                json!({"id": 7, "generation": 3, "raise": 1}),
                "raise",
            ),
            ("comp.window.raise", json!({"generation": 3}), "id"),
            (
                "comp.window.close",
                json!({"id": 7, "generation": 3, "timeout_ms": 5}),
                "timeout_ms",
            ),
            (
                "comp.window.close",
                json!({"id": 7, "generation": 3, "force": true, "timeout_ms": 60_001}),
                "timeout_ms",
            ),
            ("comp.window.place", json!({"id": 7, "generation": 3}), "x"),
            (
                "comp.window.place",
                json!({"id": 7, "generation": 3, "width": 0}),
                "width",
            ),
            (
                "comp.window.place",
                json!({"id": 7, "generation": 3, "y": "1"}),
                "y",
            ),
            ("comp.window.wait", json!({"until": "mapped"}), "match"),
            (
                "comp.window.wait",
                json!({"match": {}, "until": "mapped"}),
                "match",
            ),
            (
                "comp.window.wait",
                json!({"match": {"generation": 3}, "until": "mapped"}),
                "match.id",
            ),
            ("comp.window.wait", json!({"match": {"id": 7}}), "until"),
            (
                "comp.window.wait",
                json!({"match": {"id": 7}, "until": "resized"}),
                "until",
            ),
            (
                "comp.window.wait",
                json!({"match": {"id": 7}, "until": "size", "width": 5}),
                "width",
            ),
            (
                "comp.window.wait",
                json!({"match": {"id": 7}, "until": "mapped", "height": 5}),
                "width",
            ),
            (
                "comp.window.wait",
                json!({"match": {"id": 7}, "until": "mapped", "timeout_ms": 0}),
                "timeout_ms",
            ),
        ] {
            let body = refusal(parse_window_verb(verb, &args).expect_err("refused"));
            assert_eq!(body["error"], "invalid_value", "{verb} {args}: {body}");
            assert_eq!(body["path"], path, "{verb} {args}: {body}");
        }
        for (verb, args, field) in [
            (
                "comp.window.focus",
                json!({"id": 7, "generation": 3, "rise": true}),
                "rise",
            ),
            (
                "comp.window.close",
                json!({"id": 7, "generation": 3, "kill": true}),
                "kill",
            ),
            (
                "comp.window.place",
                json!({"id": 7, "generation": 3, "w": 5}),
                "w",
            ),
            (
                "comp.window.wait",
                json!({"match": {"appid": "x"}, "until": "mapped"}),
                "match.appid",
            ),
            (
                "comp.window.wait",
                json!({"match": {"id": 1}, "until": "mapped", "for": 1}),
                "for",
            ),
        ] {
            let body = refusal(parse_window_verb(verb, &args).expect_err("typo refused"));
            assert_eq!(body["error"], "invalid_args", "{verb}");
            assert_eq!(body["field"], field, "{verb}");
        }
    }

    /// `comp.workspace.switch` / `comp.window.send_to_workspace`: `index`
    /// is `>= 1`, `next` or `prev` (0 is `invalid_value` at the parser),
    /// `wrap` defaults on, `follow` off, and a typo is refused by name.
    #[test]
    fn workspace_verbs_parse_and_refuse_by_field() {
        assert!(window_verb("comp.workspace.switch").is_some());
        assert!(window_verb("comp.window.send_to_workspace").is_some());
        assert_eq!(
            parse_window_verb("comp.workspace.switch", &json!({"index": "next"})),
            Ok(WindowVerb::Op(WindowOp::SwitchWorkspace {
                output: None,
                index: WorkspaceIndex::Next,
                wrap: true,
            }))
        );
        assert_eq!(
            parse_window_verb(
                "comp.workspace.switch",
                &json!({"index": 3, "output": "o_x", "wrap": false})
            ),
            Ok(WindowVerb::Op(WindowOp::SwitchWorkspace {
                output: Some("o_x".into()),
                index: WorkspaceIndex::Absolute(3),
                wrap: false,
            }))
        );
        assert_eq!(
            parse_window_verb("comp.workspace.switch", &json!({"index": "prev"})),
            Ok(WindowVerb::Op(WindowOp::SwitchWorkspace {
                output: None,
                index: WorkspaceIndex::Prev,
                wrap: true,
            }))
        );
        assert_eq!(
            parse_window_verb(
                "comp.window.send_to_workspace",
                &json!({"id": 7, "generation": 3, "index": 2})
            ),
            Ok(WindowVerb::Op(WindowOp::SendToWorkspace {
                id: 7,
                generation: 3,
                index: WorkspaceIndex::Absolute(2),
                follow: false,
            }))
        );
        assert_eq!(
            parse_window_verb(
                "comp.window.send_to_workspace",
                &json!({"id": 7, "generation": 3, "index": "prev", "follow": true})
            ),
            Ok(WindowVerb::Op(WindowOp::SendToWorkspace {
                id: 7,
                generation: 3,
                index: WorkspaceIndex::Prev,
                follow: true,
            }))
        );
        for (verb, args, path) in [
            ("comp.workspace.switch", json!({}), "index"),
            ("comp.workspace.switch", json!({"index": 0}), "index"),
            ("comp.workspace.switch", json!({"index": -1}), "index"),
            ("comp.workspace.switch", json!({"index": 1.5}), "index"),
            (
                "comp.workspace.switch",
                json!({"index": "sideways"}),
                "index",
            ),
            (
                "comp.workspace.switch",
                json!({"index": 1, "wrap": "no"}),
                "wrap",
            ),
            (
                "comp.workspace.switch",
                json!({"index": 1, "output": ""}),
                "output",
            ),
            (
                "comp.workspace.switch",
                json!({"index": 1, "output": 3}),
                "output",
            ),
            (
                "comp.window.send_to_workspace",
                json!({"id": 7, "index": 2}),
                "generation",
            ),
            (
                "comp.window.send_to_workspace",
                json!({"generation": 3, "index": 2}),
                "id",
            ),
            (
                "comp.window.send_to_workspace",
                json!({"id": 7, "generation": 3}),
                "index",
            ),
            (
                "comp.window.send_to_workspace",
                json!({"id": 7, "generation": 3, "index": 0}),
                "index",
            ),
            (
                "comp.window.send_to_workspace",
                json!({"id": 7, "generation": 3, "index": 2, "follow": 1}),
                "follow",
            ),
        ] {
            let body = refusal(parse_window_verb(verb, &args).expect_err("refused"));
            assert_eq!(body["error"], "invalid_value", "{verb} {args}: {body}");
            assert_eq!(body["path"], path, "{verb} {args}: {body}");
        }
        let body = refusal(
            parse_window_verb(
                "comp.window.send_to_workspace",
                &json!({"id": 7, "index": 2}),
            )
            .expect_err("refused"),
        );
        assert_eq!(body["range"], "required (read windows.s<id>.generation)");
        for (verb, args, field) in [
            (
                "comp.workspace.switch",
                json!({"index": 1, "wrap_around": true}),
                "wrap_around",
            ),
            (
                "comp.window.send_to_workspace",
                json!({"id": 7, "generation": 3, "index": 2, "output": "o_x"}),
                "output",
            ),
        ] {
            let body = refusal(parse_window_verb(verb, &args).expect_err("typo refused"));
            assert_eq!(body["error"], "invalid_args", "{verb}");
            assert_eq!(body["field"], field, "{verb}");
        }
    }

    #[tokio::test]
    async fn region_mesh_dispatch_uses_long_pool_and_replies_once() {
        let (ingress, source, depth) = test_ingress();
        let mut responders = JoinSet::new();
        let permits = Arc::new(Semaphore::new(PORT_QUEUE_CAPACITY));
        let long_permits = Arc::new(Semaphore::new(LONG_VERB_PERMITS));
        // Model seven other long operations occupying the production pool.
        let other_operations = long_permits
            .clone()
            .try_acquire_many_owned((LONG_VERB_PERMITS - 1) as u32)
            .unwrap();
        let (reply_sender, mut replies) = tokio_mpsc::channel(8);
        let reply_timeouts = Arc::new(AtomicU64::new(0));
        for index in 0..2 {
            let mut incoming = command("comp.region.select", index);
            incoming.from = "mesh-agent".into();
            incoming.args = json!({"output":"Output-1","timeout_ms":55_000});
            incoming.body = incoming.args.to_string();
            dispatch_incoming(
                &ingress,
                &mut responders,
                &permits,
                &long_permits,
                &reply_sender,
                &reply_timeouts,
                "comp",
                incoming,
            );
        }
        let busy = replies.try_recv().unwrap();
        assert_eq!(busy.id.as_deref(), Some("1"));
        assert_eq!(busy.rc, 10);
        assert_eq!(
            serde_json::from_str::<Value>(&busy.body).unwrap()["error"],
            "busy"
        );
        assert_eq!(permits.available_permits(), PORT_QUEUE_CAPACITY);
        assert_eq!(long_permits.available_permits(), 0);
        let Ok(PortCommand::Long(mut request)) = source.try_recv() else {
            panic!("region must use LongAdmission");
        };
        assert!(
            matches!(request.op.take(),Some(LongOp::RegionSelect{output:Some(name),timeout})
            if name=="Output-1" && timeout==Duration::from_secs(55))
        );
        request.slot.take();
        assert_eq!(depth.load(Ordering::Acquire), 0);
        assert!(replies.try_recv().is_err(), "selection is still pending");
        request
            .reply
            .take()
            .unwrap()
            .send(ControlReply::Body(
                json!({"version":1,"status":"cancelled","reason":"escape"}),
            ))
            .unwrap();
        responders.join_next().await.unwrap().unwrap();
        let reply = replies.try_recv().unwrap();
        assert_eq!(reply.from, "mesh-agent");
        assert_eq!(reply.id.as_deref(), Some("0"));
        assert_eq!(reply.rc, 0);
        assert_eq!(
            serde_json::from_str::<Value>(&reply.body).unwrap()["status"],
            "cancelled"
        );
        assert!(replies.try_recv().is_err(), "exactly one terminal reply");
        assert_eq!(long_permits.available_permits(), 1);
        drop(other_operations);
        assert_eq!(long_permits.available_permits(), LONG_VERB_PERMITS);
    }

    /// noded's registry diffs reach the compositor as the full live set,
    /// unanswered (a topic delivery is not a verb); other props diffs, and a
    /// malformed set, change nothing.
    #[tokio::test]
    async fn registry_diffs_reach_the_compositor_as_the_live_set() {
        let (ingress, source, _depth) = test_ingress();
        let mut responders = JoinSet::new();
        let permits = Arc::new(Semaphore::new(PORT_QUEUE_CAPACITY));
        let long_permits = Arc::new(Semaphore::new(LONG_VERB_PERMITS));
        let (reply_sender, mut replies) = tokio_mpsc::channel(8);
        let reply_timeouts = Arc::new(AtomicU64::new(0));
        for (index, body) in [
            json!({"path":"services.registered","old":["quoin"],"new":["noded","comp-nested"]}),
            json!({"path":"mesh.peers","old":[],"new":["x"]}),
            json!({"path":"services.registered","old":[],"new":["noded", 7]}),
        ]
        .into_iter()
        .enumerate()
        {
            let mut delivery = command("props.changed", index);
            delivery.from = "noded".into();
            delivery.id = None;
            delivery.body = body.to_string();
            delivery.args = body;
            delivery.headers.insert("topic".into(), REGISTRY_TOPIC.into());
            dispatch_incoming(
                &ingress,
                &mut responders,
                &permits,
                &long_permits,
                &reply_sender,
                &reply_timeouts,
                "comp-nested",
                delivery,
            );
        }
        let Ok(PortCommand::ServicesLive(live)) = source.try_recv() else {
            panic!("the registry diff reaches the compositor");
        };
        assert_eq!(live, std::collections::BTreeSet::from(["noded".to_owned(), "comp-nested".to_owned()]));
        assert!(source.try_recv().is_err(), "nothing else is admitted");
        assert!(replies.try_recv().is_err(), "a topic delivery is never answered");
    }

    #[tokio::test]
    async fn panel_verbs_dispatch_by_literal_command_under_a_non_default_service() {
        let (ingress, source, _depth) = test_ingress();
        let mut responders = JoinSet::new();
        let permits = Arc::new(Semaphore::new(PORT_QUEUE_CAPACITY));
        let long_permits = Arc::new(Semaphore::new(LONG_VERB_PERMITS));
        let (reply_sender, mut replies) = tokio_mpsc::channel(8);
        let reply_timeouts = Arc::new(AtomicU64::new(0));
        let hold = json!({"output":"Output-1","edge":"left","surface":"quoin.panel.1",
            "holder":"popup","acquire":true});
        for (index, (verb, args)) in [
            ("comp.panel.hold", hold.clone()),
            ("comp-nested.panel.hold", hold),
            (
                "comp.panel.mode",
                json!({"output":"Output-1","edge":"left","surface":"quoin.panel.1",
                    "mode":"hidden","sticky":true}),
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let mut incoming = command(verb, index);
            incoming.body = args.to_string();
            incoming.args = args;
            dispatch_incoming(
                &ingress,
                &mut responders,
                &permits,
                &long_permits,
                &reply_sender,
                &reply_timeouts,
                "comp-nested",
                incoming,
            );
        }
        // The service-prefixed spelling is not a verb, and a typo names its field.
        let mut refusals = BTreeMap::new();
        for _ in 0..2 {
            let reply = replies.try_recv().unwrap();
            let body: Value = serde_json::from_str(&reply.body).unwrap();
            refusals.insert(reply.id.clone().unwrap(), (reply.rc, body));
        }
        assert_eq!(refusals["1"].0, 10);
        assert_eq!(refusals["1"].1["error"], "unknown_verb");
        assert_eq!(refusals["2"].0, 10);
        assert_eq!(refusals["2"].1["error"], "invalid_args");
        assert_eq!(refusals["2"].1["field"], "sticky");
        // The literal verb reaches the compositor thread under any service name.
        let Ok(PortCommand::Panel(mut request)) = source.try_recv() else {
            panic!("comp.panel.hold must be admitted");
        };
        assert_eq!(request.op.surface, "quoin.panel.1");
        assert_eq!(request.op.acquire, Some(true));
        assert!(source.try_recv().is_err(), "refused requests are never admitted");
        request
            .reply
            .take()
            .unwrap()
            .send(ControlReply::Body(json!({"accepted":true,"surface":"quoin.panel.1"})))
            .unwrap();
        responders.join_next().await.unwrap().unwrap();
        let reply = replies.try_recv().unwrap();
        assert_eq!((reply.id.as_deref(), reply.rc), (Some("0"), 0));
    }

    #[tokio::test]
    async fn window_wait_and_forced_close_take_the_long_pool() {
        let (ingress, source, depth) = test_ingress();
        let mut responders = JoinSet::new();
        let permits = Arc::new(Semaphore::new(PORT_QUEUE_CAPACITY));
        let long_permits = Arc::new(Semaphore::new(LONG_VERB_PERMITS));
        let (reply_sender, _replies) = tokio_mpsc::channel(8);
        let reply_timeouts = Arc::new(AtomicU64::new(0));
        for (index, (verb, args)) in [
            (
                "comp.window.wait",
                json!({"match": {"id": 7}, "until": "gone", "timeout_ms": 30_000}),
            ),
            (
                "comp.window.close",
                json!({"id": 7, "generation": 3, "force": true}),
            ),
            ("comp.window.close", json!({"id": 7, "generation": 3})),
        ]
        .into_iter()
        .enumerate()
        {
            let mut incoming = command(verb, index);
            incoming.body = args.to_string();
            incoming.args = args;
            dispatch_incoming(
                &ingress,
                &mut responders,
                &permits,
                &long_permits,
                &reply_sender,
                &reply_timeouts,
                "comp-nested",
                incoming,
            );
        }
        let Ok(PortCommand::Long(wait)) = source.try_recv() else {
            panic!("wait is long");
        };
        let Ok(PortCommand::Long(close)) = source.try_recv() else {
            panic!("forced close is long");
        };
        let Ok(PortCommand::Window(polite)) = source.try_recv() else {
            panic!("polite close is a window op");
        };
        assert!(matches!(wait.op, Some(LongOp::Wait(_))));
        assert!(matches!(close.op, Some(LongOp::ForceClose { .. })));
        assert_eq!(
            polite.op,
            WindowOp::Close {
                id: 7,
                generation: 3
            }
        );
        assert_eq!(long_permits.available_permits(), LONG_VERB_PERMITS - 2);
        assert_eq!(depth.load(Ordering::Acquire), 3);
        drop((wait, close));
        assert_eq!(depth.load(Ordering::Acquire), 1);
        responders.abort_all();
    }

    #[test]
    fn set_generation_is_accepted_only_on_window_leaves() {
        let (path, value, generation) =
            parse_set(&json!({"path": "windows.s7.minimized", "value": true, "generation": 3}))
                .expect("fenced window write parses");
        assert_eq!(
            (path.as_str(), value, generation),
            ("windows.s7.minimized", json!(true), Some(3))
        );
        let (_, _, generation) =
            parse_set(&json!({"path": "windows.s7.band", "value": "bottom"})).unwrap();
        assert_eq!(generation, None, "the fence is optional");
        for args in [
            json!({"path": "input.corners.enabled", "value": true, "generation": 3}),
            json!({"path": "windows.s7.minimized", "value": true, "generation": "3"}),
        ] {
            let Err((rc, body)) = parse_set(&args) else {
                panic!("{args} must be refused");
            };
            let body = serde_json::from_str::<Value>(&body).unwrap();
            assert_eq!(rc, 10);
            assert_eq!(body["error"], "invalid_value");
            assert_eq!(body["path"], "generation");
        }
    }

    #[tokio::test]
    async fn mesh_window_verbs_cross_ingress_in_order() {
        let (ingress, source, _) = test_ingress();
        let mut responders = JoinSet::new();
        let permits = Arc::new(Semaphore::new(PORT_QUEUE_CAPACITY));
        let (reply_sender, mut replies) = tokio_mpsc::channel(4);
        let reply_timeouts = Arc::new(AtomicU64::new(0));
        let mut minimize = command("comp.window.minimize", 1);
        minimize.args = json!({"id": 7, "generation": 3});
        minimize.body = minimize.args.to_string();
        minimize
            .headers
            .insert("broker_origin".into(), "mesh".into());
        let mut restore = command("comp.window.restore", 2);
        restore.args = json!({});
        restore.body = restore.args.to_string();
        let mut malformed = command("comp.window.restore", 3);
        malformed.body = "{".into();
        for command in [minimize, restore, malformed] {
            handle_incoming(
                &ingress,
                &mut responders,
                &permits,
                &reply_sender,
                &reply_timeouts,
                "comp-nested",
                command,
            );
        }
        let PortCommand::Window(first) = source.try_recv().expect("minimize admitted") else {
            panic!("window command expected");
        };
        let PortCommand::Window(second) = source.try_recv().expect("restore admitted") else {
            panic!("window command expected");
        };
        assert_eq!(
            first.op,
            WindowOp::Minimize {
                id: 7,
                generation: 3
            }
        );
        assert_eq!(second.op, WindowOp::Restore { target: None });
        assert!(first.order < second.order, "arrival order is kept");
        assert!(matches!(source.try_recv(), Err(mpsc::TryRecvError::Empty)));
        let refused = replies.recv().await.expect("malformed body refused");
        assert_eq!(refused.id.as_deref(), Some("3"));
        assert_eq!(refused.rc, 10);
        responders.abort_all();
    }

    fn move_op(target: PointerMoveTarget) -> InputOp {
        InputOp::PointerMove {
            target,
            corners: true,
        }
    }

    fn refusal(reply: ControlReply) -> Value {
        let (rc, body) = reply.into_wire();
        assert_eq!(rc, 10, "{body}");
        serde_json::from_str(&body).unwrap()
    }

    #[test]
    fn input_verbs_parse_every_documented_form() {
        assert_eq!(
            parse_input_op("comp.input.pointer.move", &json!({"x": 40, "y": 30.5})),
            Ok(move_op(PointerMoveTarget::Output {
                output: None,
                x: 40.0,
                y: 30.5
            }))
        );
        assert_eq!(
            parse_input_op(
                "comp.input.pointer.move",
                &json!({"output": "o_nested", "x": 1, "y": 2})
            ),
            Ok(move_op(PointerMoveTarget::Output {
                output: Some("o_nested".into()),
                x: 1.0,
                y: 2.0
            }))
        );
        assert_eq!(
            parse_input_op("comp.input.pointer.move", &json!({"dx": -3})),
            Ok(move_op(PointerMoveTarget::Relative { dx: -3.0, dy: 0.0 }))
        );
        assert_eq!(
            parse_input_op(
                "comp.input.pointer.move",
                &json!({"window": {"id": 7, "generation": 3}, "x": 4, "y": 5, "require_hit": true})
            ),
            Ok(move_op(PointerMoveTarget::Window {
                id: 7,
                generation: 3,
                x: 4.0,
                y: 5.0,
                require_hit: true
            }))
        );
        assert_eq!(
            parse_input_op("comp.input.pointer.button", &Value::Null),
            Ok(InputOp::PointerButton {
                button: BTN_LEFT,
                action: PressAction::Both
            })
        );
        assert_eq!(
            parse_input_op(
                "comp.input.pointer.button",
                &json!({"button": "right", "action": "press"})
            ),
            Ok(InputOp::PointerButton {
                button: BTN_RIGHT,
                action: PressAction::Press
            })
        );
        assert_eq!(
            parse_input_op(
                "comp.input.pointer.button",
                &json!({"button": 0x113, "action": "release"})
            ),
            Ok(InputOp::PointerButton {
                button: 0x113,
                action: PressAction::Release
            })
        );
        // A wheel derives detents (15 units = 120); a finger has none and a
        // missing axis stays missing.
        assert_eq!(
            parse_input_op("comp.input.pointer.scroll", &json!({"dy": 15})),
            Ok(InputOp::PointerScroll {
                dx: None,
                dy: Some(15.0),
                source: ScrollSource::Wheel,
                v120: (None, Some(120))
            })
        );
        assert_eq!(
            parse_input_op(
                "comp.input.pointer.scroll",
                &json!({"dx": 0, "source": "finger"})
            ),
            Ok(InputOp::PointerScroll {
                dx: Some(0.0),
                dy: None,
                source: ScrollSource::Finger,
                v120: (None, None)
            })
        );
        assert_eq!(
            parse_input_op(
                "comp.input.pointer.scroll",
                &json!({"dy": 10, "v120": {"dy": -240}})
            ),
            Ok(InputOp::PointerScroll {
                dx: None,
                dy: Some(10.0),
                source: ScrollSource::Wheel,
                v120: (None, Some(-240))
            })
        );
        assert_eq!(
            parse_input_op(
                "comp.input.key",
                &json!({"key": "q", "modifiers": ["super", "shift"]})
            ),
            Ok(InputOp::Key {
                key: KeySpec::Name("q".into()),
                action: PressAction::Both,
                modifiers: vec![
                    KeySpec::Name("Super_L".into()),
                    KeySpec::Name("Shift_L".into())
                ],
            })
        );
        assert_eq!(
            parse_input_op("comp.input.key", &json!({"key": 28, "action": "press"})),
            Ok(InputOp::Key {
                key: KeySpec::Evdev(28),
                action: PressAction::Press,
                modifiers: Vec::new(),
            })
        );
        assert_eq!(
            parse_input_op("comp.input.key", &json!({"text": "ok\n"})),
            Ok(InputOp::Text("ok\n".into()))
        );
        assert_eq!(
            parse_input_op("comp.input.release_all", &json!({})),
            Ok(InputOp::ReleaseAll)
        );
    }

    #[test]
    fn input_verbs_refuse_ambiguous_and_out_of_range_arguments() {
        for (verb, args, path) in [
            ("comp.input.pointer.move", json!({"x": 1}), "y"),
            (
                "comp.input.pointer.move",
                json!({"x": 1, "y": 2, "dx": 1}),
                "dx",
            ),
            (
                "comp.input.pointer.move",
                json!({"x": 1, "y": 2, "require_hit": true}),
                "require_hit",
            ),
            (
                "comp.input.pointer.move",
                json!({"window": {"id": 7}, "x": 1, "y": 2}),
                "window.generation",
            ),
            (
                "comp.input.pointer.move",
                json!({"window": {"id": 7, "generation": 1}, "dx": 1, "x": 1, "y": 2}),
                "window",
            ),
            ("comp.input.pointer.move", json!({"x": "1", "y": 2}), "x"),
            ("comp.input.pointer.move", json!({"x": 1e9, "y": 2}), "x"),
            (
                "comp.input.pointer.button",
                json!({"button": "back"}),
                "button",
            ),
            ("comp.input.pointer.button", json!({"button": 30}), "button"),
            (
                "comp.input.pointer.button",
                json!({"action": "tap"}),
                "action",
            ),
            ("comp.input.pointer.scroll", json!({}), "dy"),
            (
                "comp.input.pointer.scroll",
                json!({"dy": 1, "source": "finger", "v120": {"dy": 120}}),
                "v120",
            ),
            (
                "comp.input.pointer.scroll",
                json!({"dy": 1, "v120": {"dx": 120}}),
                "v120.dx",
            ),
            ("comp.input.key", json!({}), "key"),
            ("comp.input.key", json!({"key": ""}), "key"),
            ("comp.input.key", json!({"key": 0}), "key"),
            (
                "comp.input.key",
                json!({"key": "a", "action": "click"}),
                "action",
            ),
            (
                "comp.input.key",
                json!({"key": "a", "modifiers": ["hyper"]}),
                "modifiers",
            ),
            ("comp.input.key", json!({"text": "a", "key": "b"}), "text"),
            ("comp.input.key", json!({"text": ""}), "text"),
            ("comp.input.key", json!({"text": "x".repeat(4097)}), "text"),
            ("comp.input.release_all", json!([1]), "args"),
        ] {
            let Err(reply) = parse_input_op(verb, &args) else {
                panic!("{verb} {args} must be refused");
            };
            let body = refusal(reply);
            assert_eq!(body["error"], "invalid_value", "{verb} {args}: {body}");
            assert_eq!(body["path"], path, "{verb} {args}: {body}");
        }
        for (verb, args, field) in [
            (
                "comp.input.pointer.move",
                json!({"x": 1, "y": 2, "screen": 0}),
                "screen",
            ),
            (
                "comp.input.pointer.move",
                json!({"window": {"id": 7, "generation": 1, "gen": 1}, "x": 1, "y": 2}),
                "window.gen",
            ),
            ("comp.input.pointer.button", json!({"btn": "left"}), "btn"),
            (
                "comp.input.pointer.scroll",
                json!({"dy": 1, "discrete": 1}),
                "discrete",
            ),
            ("comp.input.key", json!({"key": "a", "mods": []}), "mods"),
            ("comp.input.release_all", json!({"all": true}), "all"),
        ] {
            let body = refusal(parse_input_op(verb, &args).expect_err("typo refused"));
            assert_eq!(body["error"], "invalid_args", "{verb}");
            assert_eq!(body["field"], field, "{verb}");
        }
    }

    #[test]
    fn sequence_parses_delays_and_names_the_failing_step() {
        let Ok(LongOp::Sequence(steps)) = parse_sequence(&json!({
            "interval_ms": 10,
            "steps": [
                {"verb": "comp.input.pointer.button", "args": {"action": "press"}, "delay_ms": 0},
                {"verb": "comp.input.pointer.move", "args": {"dx": 5}},
                {"verb": "comp.input.release_all"},
            ],
        })) else {
            panic!("sequence parses");
        };
        assert_eq!(
            steps
                .iter()
                .map(|step| (step.verb, step.delay))
                .collect::<Vec<_>>(),
            [
                ("comp.input.pointer.button", Duration::ZERO),
                ("comp.input.pointer.move", Duration::from_millis(10)),
                ("comp.input.release_all", Duration::from_millis(10)),
            ]
        );
        assert_eq!(LongOp::Sequence(steps).budget(), Duration::from_millis(20));

        let body = refusal(
            parse_sequence(&json!({"steps": [
                {"verb": "comp.input.key", "args": {"key": "a"}},
                {"verb": "comp.input.key", "args": {"key": "a", "action": "hold"}},
            ]}))
            .expect_err("bad step refused"),
        );
        assert_eq!(body["path"], "steps[1].args.action");
        let body = refusal(
            parse_sequence(&json!({"steps": [{"verb": "comp.input.key", "args": {"k": 1}}]}))
                .expect_err("bad step field refused"),
        );
        assert_eq!(body["field"], "steps[0].args.k");
        for (args, path) in [
            (json!({"steps": []}), "steps"),
            (
                json!({"steps": [{"verb": "comp.window.focus"}]}),
                "steps.verb",
            ),
            (
                json!({"steps": [{"verb": "comp.input.sequence"}]}),
                "steps.verb",
            ),
            (
                json!({"steps": [
                    {"verb": "comp.input.release_all", "delay_ms": 40_000},
                    {"verb": "comp.input.release_all", "delay_ms": 30_000},
                ]}),
                "steps",
            ),
            (
                json!({"steps": [{"verb": "comp.input.release_all"}], "interval_ms": -1}),
                "interval_ms",
            ),
        ] {
            let body = refusal(parse_sequence(&args).expect_err("refused"));
            assert_eq!(body["path"], path, "{args}");
        }
        let too_many = vec![json!({"verb": "comp.input.release_all"}); SEQUENCE_MAX_STEPS + 1];
        let body = refusal(parse_sequence(&json!({"steps": too_many})).expect_err("capped"));
        assert_eq!(body["path"], "steps");
        let body = refusal(
            parse_sequence(&json!({"steps": [{"verb": "comp.input.release_all", "wait": 1}]}))
                .expect_err("unknown step field"),
        );
        assert_eq!(body["field"], "steps[0].wait");
    }

    #[tokio::test]
    async fn input_verbs_cross_ingress_in_order_and_long_verbs_release_their_slot() {
        let (ingress, source, depth) = test_ingress();
        let mut responders = JoinSet::new();
        let permits = Arc::new(Semaphore::new(PORT_QUEUE_CAPACITY));
        let long_permits = Arc::new(Semaphore::new(1));
        let (reply_sender, mut replies) = tokio_mpsc::channel(8);
        let reply_timeouts = Arc::new(AtomicU64::new(0));
        let mut dispatch = |verb: &str, id: usize, args: Value| {
            let mut incoming = command(verb, id);
            incoming.body = args.to_string();
            incoming.args = args;
            incoming
                .headers
                .insert("broker_origin".into(), "mesh".into());
            dispatch_incoming(
                &ingress,
                &mut responders,
                &permits,
                &long_permits,
                &reply_sender,
                &reply_timeouts,
                "comp-nested",
                incoming,
            );
        };
        dispatch("comp.input.pointer.move", 1, json!({"x": 1, "y": 2}));
        dispatch(
            "comp.input.sequence",
            2,
            json!({"steps": [{"verb": "comp.input.release_all", "delay_ms": 50}]}),
        );
        // The long pool has one permit and it is held.
        dispatch(
            "comp.input.sequence",
            3,
            json!({"steps": [{"verb": "comp.input.release_all"}]}),
        );
        dispatch("comp.input.key", 4, json!({"text": "ok"}));
        dispatch("comp.input.teleport", 5, json!({}));
        dispatch("comp.input.key", 6, json!({"key": "a", "hold": true}));

        let Ok(PortCommand::Input(first)) = source.try_recv() else {
            panic!("move admitted");
        };
        let Ok(PortCommand::Long(mut second)) = source.try_recv() else {
            panic!("sequence admitted");
        };
        let Ok(PortCommand::Input(third)) = source.try_recv() else {
            panic!("text admitted");
        };
        assert!(first.order < second.order && second.order < third.order);
        assert_eq!(third.op, InputOp::Text("ok".into()));
        assert!(matches!(source.try_recv(), Err(mpsc::TryRecvError::Empty)));
        assert_eq!(depth.load(Ordering::Acquire), 3);
        // Taking the long request off the queue frees its slot while the
        // verb itself keeps waiting.
        assert!(second.slot.take().is_some());
        assert_eq!(depth.load(Ordering::Acquire), 2);

        let mut refusals = BTreeMap::new();
        for _ in 0..3 {
            let reply = replies.recv().await.expect("refusal");
            refusals.insert(
                reply.id.clone().unwrap(),
                serde_json::from_str::<Value>(&reply.body).unwrap(),
            );
        }
        assert_eq!(refusals["3"]["error"], "busy");
        assert_eq!(refusals["5"]["error"], "unknown_verb");
        assert_eq!(refusals["6"]["error"], "invalid_args");

        // The long reply waits for its own budget, not the 2 s snapshot one.
        let _ = second
            .reply
            .take()
            .unwrap()
            .send(ControlReply::Body(json!({"steps": []})));
        let reply = replies.recv().await.expect("sequence reply");
        assert_eq!(reply.id.as_deref(), Some("2"));
        assert_eq!(reply.rc, 0);
        responders.abort_all();
    }

    #[test]
    fn one_verb_is_capped_at_4096_injected_events() {
        let text = "a".repeat(TEXT_MAX_CHARS);
        let step = json!({"verb": "comp.input.key", "args": {"text": text}});
        // 4 x 256 x 4 = 4096 fits exactly; one more event does not.
        let Ok(LongOp::Sequence(steps)) = parse_sequence(&json!({"steps": vec![step.clone(); 4]}))
        else {
            panic!("exactly at the cap parses");
        };
        assert_eq!(
            steps
                .iter()
                .map(|step| step.op.event_bound())
                .sum::<usize>(),
            MAX_EVENTS_PER_VERB
        );
        let mut over = vec![step; 4];
        over.push(json!({"verb": "comp.input.pointer.move", "args": {"dx": 1}}));
        let body = refusal(parse_sequence(&json!({"steps": over})).expect_err("over the cap"));
        assert_eq!(body["path"], "steps");
        assert!(body["range"].as_str().unwrap().contains("4096"), "{body}");
        let body = refusal(
            parse_input_op(
                "comp.input.key",
                &json!({"text": "a".repeat(TEXT_MAX_CHARS + 1)}),
            )
            .expect_err("text over the cap"),
        );
        assert_eq!(body["path"], "text");
        assert_eq!(
            parse_input_op(
                "comp.input.pointer.move",
                &json!({"dx": 1, "corners": false})
            ),
            Ok(InputOp::PointerMove {
                target: PointerMoveTarget::Relative { dx: 1.0, dy: 0.0 },
                corners: false
            })
        );
        let body = refusal(
            parse_window_verb(
                "comp.window.wait",
                &json!({"match": {"id": 7, "app_id": "a"}, "until": "mapped"}),
            )
            .expect_err("id with names"),
        );
        assert_eq!(body["path"], "match");
    }

    #[test]
    fn long_admission_budget_is_the_verb_deadline_plus_slack() {
        let (ingress, _source, _) = test_ingress();
        let admission = ingress
            .request_long(LongOp::Sequence(vec![SequenceStep {
                verb: "comp.input.release_all",
                op: InputOp::ReleaseAll,
                delay: Duration::from_millis(1500),
            }]))
            .expect("admitted");
        assert_eq!(
            admission.timeout_for_test(),
            Duration::from_millis(1500) + LONG_VERB_SLACK
        );
    }

    #[test]
    fn refused_reply_carries_code_and_detail() {
        assert_eq!(
            ControlReply::refused("occluded", json!({"id": 7, "error": "ignored"})).into_wire(),
            (10, Arc::from(r#"{"error":"occluded","id":7}"#))
        );
        assert_eq!(
            ControlReply::refused("busy", Value::Null).into_wire(),
            (10, Arc::from(r#"{"error":"busy"}"#))
        );
    }

    #[tokio::test]
    async fn authorised_set_crosses_ingress_and_preserves_response_correlation() {
        let (ingress, source, _) = test_ingress();
        let mut responders = JoinSet::new();
        let permits = Arc::new(Semaphore::new(PORT_QUEUE_CAPACITY));
        let (reply_sender, mut replies) = tokio_mpsc::channel(2);
        let reply_timeouts = Arc::new(AtomicU64::new(0));
        let command = local_set_command(37, "input.corners.dwell_ms", json!(250));

        handle_incoming(
            &ingress,
            &mut responders,
            &permits,
            &reply_sender,
            &reply_timeouts,
            "comp-nested",
            command,
        );
        let PortCommand::Set(request) = source.try_recv().expect("set admitted") else {
            panic!("set command expected");
        };
        assert_eq!(request.path, "input.corners.dwell_ms");
        assert_eq!(request.value, json!(250));
        request
            .reply
            .expect("set reply sender")
            .send(ControlReply::Set {
                path: "input.corners.dwell_ms".into(),
                old: PropValue::U64(200),
                new: PropValue::U64(250),
                persisted: None,
            })
            .expect("responder remains live");
        responders
            .join_next()
            .await
            .expect("responder completes")
            .expect("task");
        let reply = replies.recv().await.expect("correlated reply");
        assert_eq!(reply.id.as_deref(), Some("37"));
        assert_eq!(reply.command, "comp.props.set");
        assert_eq!(reply.rc, 0);
    }

    #[tokio::test]
    async fn invalid_sets_cannot_exhaust_ingress_or_responder_permits() {
        let (ingress, source, _) = test_ingress();
        let mut responders = JoinSet::new();
        let permits = Arc::new(Semaphore::new(PORT_QUEUE_CAPACITY));
        let (reply_sender, mut replies) = tokio_mpsc::channel(PORT_QUEUE_CAPACITY + 1);
        let reply_timeouts = Arc::new(AtomicU64::new(0));

        for id in 0..PORT_QUEUE_CAPACITY {
            handle_incoming(
                &ingress,
                &mut responders,
                &permits,
                &reply_sender,
                &reply_timeouts,
                "comp-nested",
                local_set_command(id, "input.corners.dwell_ms", json!(5001)),
            );
        }
        for _ in 0..PORT_QUEUE_CAPACITY {
            let reply = replies.recv().await.expect("invalid-value reply");
            assert_eq!(reply.rc, 10);
            assert_eq!(
                serde_json::from_str::<Value>(&reply.body).unwrap()["error"],
                "invalid_value"
            );
        }
        assert!(responders.is_empty());
        assert_eq!(permits.available_permits(), PORT_QUEUE_CAPACITY);
        assert_eq!(ingress.depth_for_test(), 0);
        assert!(matches!(source.try_recv(), Err(mpsc::TryRecvError::Empty)));

        handle_incoming(
            &ingress,
            &mut responders,
            &permits,
            &reply_sender,
            &reply_timeouts,
            "comp-nested",
            command("comp.info", PORT_QUEUE_CAPACITY),
        );
        assert!(matches!(source.try_recv(), Ok(PortCommand::Snapshot(_))));
    }

    #[tokio::test]
    async fn non_finite_json_set_is_rejected_before_admission() {
        let (ingress, source, _) = test_ingress();
        let mut responders = JoinSet::new();
        let permits = Arc::new(Semaphore::new(PORT_QUEUE_CAPACITY));
        let (reply_sender, mut replies) = tokio_mpsc::channel(2);
        let reply_timeouts = Arc::new(AtomicU64::new(0));
        let mut command = local_set_command(3, "input.corners.velocity_max_px_s", json!(1500.0));
        command.body = "{\"path\":\"input.corners.velocity_max_px_s\",\"value\":NaN}".into();
        handle_incoming(
            &ingress,
            &mut responders,
            &permits,
            &reply_sender,
            &reply_timeouts,
            "comp-nested",
            command,
        );
        let reply = replies.recv().await.expect("invalid-value reply");
        assert_eq!(reply.rc, 10);
        assert_eq!(
            serde_json::from_str::<Value>(&reply.body).unwrap()["error"],
            "invalid_value"
        );
        assert!(matches!(source.try_recv(), Err(mpsc::TryRecvError::Empty)));
    }

    #[tokio::test]
    async fn exact_noded_topic_lifecycle_notices_cross_as_watch_state_only() {
        let (ingress, source, _) = test_ingress();
        let mut responders = JoinSet::new();
        let permits = Arc::new(Semaphore::new(PORT_QUEUE_CAPACITY));
        let (reply_sender, _replies) = tokio_mpsc::channel(2);
        let reply_timeouts = Arc::new(AtomicU64::new(0));
        for (verb, active) in [("topic.active", true), ("topic.idle", false)] {
            let mut command = command(verb, 1);
            command.from = "noded".into();
            command
                .headers
                .insert("name".into(), "comp-nested.props.changed".into());
            handle_incoming(
                &ingress,
                &mut responders,
                &permits,
                &reply_sender,
                &reply_timeouts,
                "comp-nested",
                command,
            );
            let PortCommand::WatchState {
                active: observed, ..
            } = source.try_recv().expect("notice staged")
            else {
                panic!("watch-state command expected");
            };
            assert_eq!(observed, active);
        }
    }

    #[test]
    fn both_lifecycle_directions_coalesce_latest_wins_when_ingress_is_full() {
        let (ingress, _source, _) = test_ingress();
        let _admissions = (0..PORT_QUEUE_CAPACITY)
            .map(|_| ingress.request_snapshot().expect("fill ingress"))
            .collect::<Vec<_>>();
        ingress.set_watch_state(false);
        let first_idle = ingress.pending_idle_order.load(Ordering::Acquire);
        ingress.set_watch_state(true);
        let active = ingress.pending_active_order.load(Ordering::Acquire);
        ingress.set_watch_state(false);
        let final_idle = ingress.pending_idle_order.load(Ordering::Acquire);
        assert_ne!(first_idle, 0);
        assert!(first_idle < active && active < final_idle);
    }

    #[tokio::test]
    async fn saturated_ingress_returns_busy_before_set_reaches_calloop() {
        let (ingress, source, _) = test_ingress();
        let _admissions = (0..PORT_QUEUE_CAPACITY)
            .map(|_| ingress.request_snapshot().expect("fill ingress"))
            .collect::<Vec<_>>();
        let mut responders = JoinSet::new();
        let permits = Arc::new(Semaphore::new(PORT_QUEUE_CAPACITY));
        let (reply_sender, mut replies) = tokio_mpsc::channel(2);
        let reply_timeouts = Arc::new(AtomicU64::new(0));
        handle_incoming(
            &ingress,
            &mut responders,
            &permits,
            &reply_sender,
            &reply_timeouts,
            "comp-nested",
            local_set_command(3, "input.corners.dwell_ms", json!(250)),
        );
        let reply = replies.recv().await.expect("busy reply");
        assert_eq!(
            reply.body.as_ref(),
            "{\"error\":\"busy\",\"error_code\":\"busy\"}"
        );
        for _ in 0..PORT_QUEUE_CAPACITY {
            assert!(matches!(source.try_recv(), Ok(PortCommand::Snapshot(_))));
        }
    }

    #[tokio::test]
    async fn publisher_gaps_each_topic_no_later_than_its_next_record_or_idle_flush() {
        let lost = Arc::new(AtomicU64::new(0));
        let (mut producer, receiver) = port_observation::test_outbox(Arc::clone(&lost), 2);
        let notifier = producer.notifier();
        producer.offer(ObservationRecord::PropsChanged {
            path: "input.corners.enabled".into(),
            old: PropValue::Bool(true),
            new: PropValue::Bool(false),
            unix_ms: 0,
            cause: "props.set",
            event_seq: 1,
        });
        for event_seq in 2..=4 {
            producer.offer(ObservationRecord::FocusChanged {
                keyboard: Some(event_seq),
                previous: None,
                exclusive_latch: None,
                event_seq,
            });
        }
        let (client, _commands, _states) = FakeClient::new(ConnState::Connected, false);
        let publications = Arc::clone(&client.publications);
        let publish_timeouts = Arc::new(AtomicU64::new(0));
        let (shutdown_tx, shutdown) = watch::channel(false);
        let task = tokio::spawn(publisher_loop(
            Arc::new(client),
            Arc::from("comp-nested"),
            receiver,
            notifier,
            Arc::clone(&lost),
            Arc::clone(&publish_timeouts),
            shutdown,
        ));
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if publications
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .len()
                    >= 4
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("both bounded-lane survivors and both affected-topic gaps publish");
        shutdown_tx.send_replace(true);
        task.await.expect("publisher exits");

        let published = publications
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let first_survivor =
            cosmix_bus::bus::parse(&published[0].1).expect("first survivor parses");
        assert_eq!(
            published[0].0.get("name").map(String::as_str),
            Some("comp-nested.focus.changed")
        );
        assert_eq!(first_survivor.get("event_seq"), Some("3"));

        let focus_gap = cosmix_bus::bus::parse(&published[1].1).expect("focus gap parses");
        assert_eq!(
            published[1].0.get("name").map(String::as_str),
            Some("comp-nested.focus.changed")
        );
        assert_eq!(focus_gap.get("command"), Some("focus.changed"));
        assert_eq!(focus_gap.get("event_seq"), Some("2"));
        assert_eq!(
            serde_json::from_str::<Value>(&focus_gap.body).unwrap(),
            json!({"gap": true, "lost_count": 2, "cause": "outbox.overflow"})
        );

        let second_survivor =
            cosmix_bus::bus::parse(&published[2].1).expect("second survivor parses");
        assert_eq!(second_survivor.get("event_seq"), Some("4"));

        let props_gap = cosmix_bus::bus::parse(&published[3].1).expect("props gap parses");
        assert_eq!(
            published[3].0.get("name").map(String::as_str),
            Some("comp-nested.props.changed")
        );
        assert_eq!(props_gap.get("command"), Some("props.changed"));
        assert_eq!(props_gap.get("event_seq"), Some("2"));
        assert_eq!(
            serde_json::from_str::<Value>(&props_gap.body).unwrap(),
            json!({"gap": true, "lost_count": 2, "cause": "outbox.overflow"})
        );
        assert_eq!(publish_timeouts.load(Ordering::Acquire), 0);
    }

    #[tokio::test]
    async fn consecutive_carried_intervals_coalesce_to_one_gap_before_the_survivor() {
        let lost = Arc::new(AtomicU64::new(0));
        let (mut producer, receiver) = port_observation::test_outbox(Arc::clone(&lost), 1);
        let notifier = producer.notifier();
        for event_seq in 1..=4 {
            producer.offer(ObservationRecord::FocusChanged {
                keyboard: Some(event_seq),
                previous: None,
                exclusive_latch: None,
                event_seq,
            });
        }
        let (client, _commands, _states) = FakeClient::new(ConnState::Connected, false);
        let publications = Arc::clone(&client.publications);
        let (shutdown_tx, shutdown) = watch::channel(false);
        let task = tokio::spawn(publisher_loop(
            Arc::new(client),
            Arc::from("comp-nested"),
            receiver,
            notifier,
            Arc::clone(&lost),
            Arc::new(AtomicU64::new(0)),
            shutdown,
        ));
        tokio::time::timeout(Duration::from_secs(1), async {
            while publications
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .len()
                != 2
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("one coalesced gap and the sole survivor publish");
        shutdown_tx.send_replace(true);
        task.await.expect("publisher exits");

        let published = publications
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let gap = cosmix_bus::bus::parse(&published[0].1).expect("gap parses");
        assert_eq!(gap.get("event_seq"), Some("3"));
        assert_eq!(
            serde_json::from_str::<Value>(&gap.body).unwrap(),
            json!({"gap": true, "lost_count": 3, "cause": "outbox.overflow"})
        );
        let survivor = cosmix_bus::bus::parse(&published[1].1).expect("survivor parses");
        assert_eq!(survivor.get("event_seq"), Some("4"));
        assert_eq!(lost.load(Ordering::Acquire), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn failed_idle_gap_retries_on_broker_reconnect_without_a_new_record() {
        let lost = Arc::new(AtomicU64::new(0));
        let (mut producer, receiver) = port_observation::test_outbox(Arc::clone(&lost), 1);
        let notifier = producer.notifier();
        producer.offer(ObservationRecord::PropsChanged {
            path: "input.corners.enabled".into(),
            old: PropValue::Bool(true),
            new: PropValue::Bool(false),
            unix_ms: 0,
            cause: "props.set",
            event_seq: 1,
        });
        producer.offer(ObservationRecord::FocusChanged {
            keyboard: Some(2),
            previous: None,
            exclusive_latch: None,
            event_seq: 2,
        });
        notifier.notified().await;

        let (ingress, _source, _) = test_ingress();
        let broker = Arc::new(AtomicU8::new(BROKER_RETRYING));
        let reply_timeouts = Arc::new(AtomicU64::new(0));
        let publish_timeouts = Arc::new(AtomicU64::new(0));
        let (shutdown_tx, shutdown) = watch::channel(false);
        let (client, _commands, states) = FakeClient::new(ConnState::Connected, false);
        client.reject_publish_attempt.store(2, Ordering::Release);
        let publish_mode = Arc::clone(&client.publish_mode);
        let publications = Arc::clone(&client.publications);
        let mut client = Some(client);
        let worker = tokio::spawn(worker_loop(
            "comp-nested".into(),
            ingress,
            Arc::clone(&broker),
            reply_timeouts,
            Arc::clone(&publish_timeouts),
            receiver,
            notifier,
            Arc::clone(&lost),
            shutdown,
            move || future::ready(Ok(client.take().expect("one connection attempt"))),
        ));

        wait_for_broker(&broker, BROKER_CONNECTED).await;
        tokio::time::timeout(Duration::from_secs(1), async {
            while publish_timeouts.load(Ordering::Acquire) != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the idle-flush props gap fails once");
        assert_eq!(
            publications
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .len(),
            1,
            "only the focus survivor publishes before broker recovery"
        );

        publish_mode.store(1, Ordering::Release);
        states.send_replace(ConnState::Disconnected);
        wait_for_broker(&broker, BROKER_RETRYING).await;
        tokio::time::timeout(Duration::from_secs(1), async {
            while publish_timeouts.load(Ordering::Acquire) != 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the disconnected edge wakes the retained gap retry");

        publish_mode.store(0, Ordering::Release);
        states.send_replace(ConnState::Connected);
        wait_for_broker(&broker, BROKER_CONNECTED).await;
        tokio::time::timeout(Duration::from_secs(1), async {
            while publications
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .len()
                != 2
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the reconnect edge publishes the retained gap without new data");

        shutdown_tx.send_replace(true);
        worker.await.expect("worker exits cleanly");
        let published = publications
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let survivor = cosmix_bus::bus::parse(&published[0].1).expect("survivor parses");
        assert_eq!(survivor.get("event_seq"), Some("2"));
        let gap = cosmix_bus::bus::parse(&published[1].1).expect("gap parses");
        assert_eq!(gap.get("command"), Some("props.changed"));
        assert_eq!(gap.get("event_seq"), Some("1"));
        assert_eq!(
            serde_json::from_str::<Value>(&gap.body).unwrap(),
            json!({"gap": true, "lost_count": 1, "cause": "outbox.overflow"})
        );
        assert_eq!(lost.load(Ordering::Acquire), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn failed_gap_after_event_sequence_exhaustion_retries_on_backoff_without_data() {
        let lost = Arc::new(AtomicU64::new(0));
        let (mut producer, receiver) = port_observation::test_outbox(Arc::clone(&lost), 1);
        let notifier = producer.notifier();
        producer.offer(ObservationRecord::PropsChanged {
            path: "input.corners.enabled".into(),
            old: PropValue::Bool(true),
            new: PropValue::Bool(false),
            unix_ms: 0,
            cause: "props.set",
            event_seq: u64::MAX - 1,
        });
        producer.offer(ObservationRecord::FocusChanged {
            keyboard: None,
            previous: Some(1),
            exclusive_latch: None,
            event_seq: u64::MAX,
        });
        notifier.notified().await;

        let (client, _commands, _states) = FakeClient::new(ConnState::Connected, false);
        client.reject_publish_attempt.store(2, Ordering::Release);
        let publish_attempts = Arc::clone(&client.publish_attempts);
        let publications = Arc::clone(&client.publications);
        let publish_timeouts = Arc::new(AtomicU64::new(0));
        let (shutdown_tx, shutdown) = watch::channel(false);
        let task = tokio::spawn(publisher_loop(
            Arc::new(client),
            Arc::from("comp-nested"),
            receiver,
            notifier,
            Arc::clone(&lost),
            Arc::clone(&publish_timeouts),
            shutdown,
        ));
        while publish_timeouts.load(Ordering::Acquire) != 1 {
            tokio::task::yield_now().await;
        }
        tokio::task::yield_now().await;
        assert_eq!(publish_attempts.load(Ordering::Acquire), 2);
        assert_eq!(
            publications
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .len(),
            1
        );

        tokio::time::advance(Duration::from_millis(999)).await;
        tokio::task::yield_now().await;
        assert_eq!(
            publish_attempts.load(Ordering::Acquire),
            2,
            "the failed gap waits for its one-second first backoff"
        );
        tokio::time::advance(Duration::from_millis(1)).await;
        while publications
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
            != 2
        {
            tokio::task::yield_now().await;
        }

        shutdown_tx.send_replace(true);
        task.await.expect("publisher exits");
        let published = publications
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let exhausted_sequence = u64::MAX.to_string();
        let last_lost_sequence = (u64::MAX - 1).to_string();
        let survivor = cosmix_bus::bus::parse(&published[0].1).expect("survivor parses");
        assert_eq!(survivor.get("event_seq"), Some(exhausted_sequence.as_str()));
        let gap = cosmix_bus::bus::parse(&published[1].1).expect("gap parses");
        assert_eq!(gap.get("command"), Some("props.changed"));
        assert_eq!(gap.get("event_seq"), Some(last_lost_sequence.as_str()));
        assert_eq!(publish_attempts.load(Ordering::Acquire), 3);
        assert_eq!(publish_timeouts.load(Ordering::Acquire), 1);
        assert_eq!(lost.load(Ordering::Acquire), 1);
    }

    #[tokio::test]
    #[should_panic(expected = "observation producer disconnected before port shutdown")]
    async fn observation_lane_disconnect_without_shutdown_violates_lifecycle() {
        let lost = Arc::new(AtomicU64::new(0));
        let (producer, receiver) = port_observation::outbox(Arc::clone(&lost));
        let notifier = producer.notifier();
        drop(producer);
        let (client, _commands, _states) = FakeClient::new(ConnState::Connected, false);
        let (_shutdown_tx, shutdown) = watch::channel(false);
        publisher_loop(
            Arc::new(client),
            Arc::from("comp-nested"),
            receiver,
            notifier,
            lost,
            Arc::new(AtomicU64::new(0)),
            shutdown,
        )
        .await;
    }

    #[test]
    fn failed_gap_retry_backoff_doubles_and_caps_at_thirty_seconds() {
        let mut delay = None;
        for expected in [1, 2, 4, 8, 16, 30, 30] {
            arm_gap_retry(&mut delay);
            assert_eq!(delay, Some(Duration::from_secs(expected)));
        }
    }

    #[tokio::test]
    async fn successful_gap_topics_are_not_republished_after_a_later_gap_fails() {
        let lost = Arc::new(AtomicU64::new(0));
        let (mut producer, receiver) = port_observation::test_outbox(Arc::clone(&lost), 2);
        let notifier = producer.notifier();
        producer.offer(ObservationRecord::PropsChanged {
            path: "input.corners.enabled".into(),
            old: PropValue::Bool(true),
            new: PropValue::Bool(false),
            unix_ms: 0,
            cause: "props.set",
            event_seq: 1,
        });
        producer.offer(ObservationRecord::FocusChanged {
            keyboard: Some(2),
            previous: None,
            exclusive_latch: None,
            event_seq: 2,
        });
        for event_seq in 3..=4 {
            producer.offer(ObservationRecord::SurfaceMapped {
                id: event_seq,
                role: "toplevel".into(),
                foreign_id: None,
                window: Default::default(),
                event_seq,
            });
        }
        let (client, _commands, _states) = FakeClient::new(ConnState::Connected, false);
        client.reject_publish_attempt.store(4, Ordering::Release);
        let publications = Arc::clone(&client.publications);
        let publish_timeouts = Arc::new(AtomicU64::new(0));
        let (shutdown_tx, shutdown) = watch::channel(false);
        let task = tokio::spawn(publisher_loop(
            Arc::new(client),
            Arc::from("comp-nested"),
            receiver,
            notifier,
            Arc::clone(&lost),
            Arc::clone(&publish_timeouts),
            shutdown,
        ));
        tokio::time::timeout(Duration::from_secs(1), async {
            while publish_timeouts.load(Ordering::Acquire) != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the later idle-flush focus gap fails after the props gap succeeds");
        producer.offer(ObservationRecord::FocusChanged {
            keyboard: Some(5),
            previous: Some(4),
            exclusive_latch: None,
            event_seq: 5,
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            while publications
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .len()
                != 5
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("remaining focus gap and survivor publish");
        shutdown_tx.send_replace(true);
        task.await.expect("publisher exits");

        let published = publications
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert_eq!(
            published
                .iter()
                .filter(|(headers, _)| headers
                    .get("name")
                    .is_some_and(|name| name.ends_with("props.changed")))
                .count(),
            1,
            "the successful props gap is acknowledged before the focus gap fails"
        );
        assert_eq!(lost.load(Ordering::Acquire), 2);
        let first_survivor =
            cosmix_bus::bus::parse(&published[0].1).expect("first survivor parses");
        assert_eq!(first_survivor.get("event_seq"), Some("3"));
        let second_survivor =
            cosmix_bus::bus::parse(&published[1].1).expect("second survivor parses");
        assert_eq!(second_survivor.get("event_seq"), Some("4"));
        let props_gap = cosmix_bus::bus::parse(&published[2].1).expect("props gap parses");
        assert_eq!(props_gap.get("event_seq"), Some("2"));
        let focus_gap = cosmix_bus::bus::parse(&published[3].1).expect("focus gap parses");
        assert_eq!(focus_gap.get("event_seq"), Some("2"));
        assert_eq!(
            serde_json::from_str::<Value>(&focus_gap.body).unwrap(),
            json!({"gap": true, "lost_count": 2, "cause": "outbox.overflow"})
        );
        let survivor = cosmix_bus::bus::parse(&published[4].1).expect("survivor parses");
        assert_eq!(survivor.get("event_seq"), Some("5"));
        assert_eq!(publish_timeouts.load(Ordering::Acquire), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn idle_publisher_has_no_retry_timer_when_nothing_is_pending() {
        let lost = Arc::new(AtomicU64::new(0));
        let (mut producer, receiver) = port_observation::outbox(Arc::clone(&lost));
        let notifier = producer.notifier();
        let (client, _commands, _states) = FakeClient::new(ConnState::Connected, false);
        let publish_attempts = Arc::clone(&client.publish_attempts);
        let publications = Arc::clone(&client.publications);
        let (shutdown_tx, shutdown) = watch::channel(false);
        let task = tokio::spawn(publisher_loop(
            Arc::new(client),
            Arc::from("comp-nested"),
            receiver,
            notifier,
            lost,
            Arc::new(AtomicU64::new(0)),
            shutdown,
        ));
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(3_600)).await;
        tokio::task::yield_now().await;
        assert_eq!(publish_attempts.load(Ordering::Acquire), 0);
        assert!(
            publications
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_empty()
        );
        tokio::time::advance(Duration::from_secs(3_600)).await;
        tokio::task::yield_now().await;
        assert_eq!(
            publish_attempts.load(Ordering::Acquire),
            0,
            "idle publisher performs no timer-driven publication attempt"
        );
        assert!(
            publications
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_empty(),
            "idle publisher has no timer or polling wake"
        );

        producer.offer(ObservationRecord::FocusChanged {
            keyboard: Some(1),
            previous: None,
            exclusive_latch: None,
            event_seq: 1,
        });
        tokio::time::timeout(Duration::from_millis(1), async {
            while publications
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_empty()
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("successful offer wakes idle publisher without advancing time");
        shutdown_tx.send_replace(true);
        task.await.expect("publisher exits");
    }

    #[tokio::test]
    async fn rejected_publication_gaps_every_topic_in_the_discarded_backlog() {
        let lost = Arc::new(AtomicU64::new(0));
        let (mut producer, receiver) = port_observation::outbox(Arc::clone(&lost));
        let notifier = producer.notifier();
        producer.offer(ObservationRecord::PropsChanged {
            path: "input.corners.enabled".into(),
            old: PropValue::Bool(true),
            new: PropValue::Bool(false),
            unix_ms: 0,
            cause: "props.set",
            event_seq: 1,
        });
        producer.offer(ObservationRecord::FocusChanged {
            keyboard: Some(2),
            previous: Some(1),
            exclusive_latch: None,
            event_seq: 2,
        });
        let (client, _commands, _states) = FakeClient::new(ConnState::Connected, false);
        client.publish_mode.store(1, Ordering::Release);
        let mode = Arc::clone(&client.publish_mode);
        let publications = Arc::clone(&client.publications);
        let publish_timeouts = Arc::new(AtomicU64::new(0));
        let (shutdown_tx, shutdown) = watch::channel(false);
        let task = tokio::spawn(publisher_loop(
            Arc::new(client),
            Arc::from("comp-nested"),
            receiver,
            notifier,
            Arc::clone(&lost),
            Arc::clone(&publish_timeouts),
            shutdown,
        ));
        tokio::time::timeout(Duration::from_secs(1), async {
            while lost.load(Ordering::Acquire) != 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("failed record and backlog become loss");
        mode.store(0, Ordering::Release);
        producer.offer(ObservationRecord::FocusChanged {
            keyboard: Some(3),
            previous: Some(2),
            exclusive_latch: None,
            event_seq: 3,
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            while publications
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .len()
                != 3
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("publisher-loss gaps and survivor publish");
        shutdown_tx.send_replace(true);
        task.await.expect("publisher exits");
        let published = publications
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let focus_gap = cosmix_bus::bus::parse(&published[0].1).expect("focus gap parses");
        assert_eq!(
            published[0].0.get("name").map(String::as_str),
            Some("comp-nested.focus.changed")
        );
        assert_eq!(focus_gap.get("event_seq"), Some("2"));
        assert_eq!(
            serde_json::from_str::<Value>(&focus_gap.body).unwrap(),
            json!({"gap": true, "lost_count": 2, "cause": "publisher.loss"})
        );
        let survivor = cosmix_bus::bus::parse(&published[1].1).expect("survivor parses");
        assert_eq!(survivor.get("command"), Some("focus.changed"));
        assert_eq!(survivor.get("event_seq"), Some("3"));
        let props_gap = cosmix_bus::bus::parse(&published[2].1).expect("props gap parses");
        assert_eq!(
            published[2].0.get("name").map(String::as_str),
            Some("comp-nested.props.changed")
        );
        assert_eq!(props_gap.get("event_seq"), Some("2"));
        assert_eq!(
            serde_json::from_str::<Value>(&props_gap.body).unwrap(),
            json!({"gap": true, "lost_count": 2, "cause": "publisher.loss"})
        );
        assert_eq!(publish_timeouts.load(Ordering::Acquire), 1);
    }

    #[tokio::test]
    async fn overflow_after_failed_backlog_drain_still_gaps_its_topics() {
        let lost = Arc::new(AtomicU64::new(0));
        let (mut producer, receiver) = port_observation::test_outbox(Arc::clone(&lost), 2);
        let notifier = producer.notifier();
        producer.offer(ObservationRecord::FocusChanged {
            keyboard: Some(1),
            previous: None,
            exclusive_latch: None,
            event_seq: 1,
        });
        producer.offer(ObservationRecord::FocusChanged {
            keyboard: Some(2),
            previous: Some(1),
            exclusive_latch: None,
            event_seq: 2,
        });
        let (client, _commands, _states) = FakeClient::new(ConnState::Connected, false);
        client.publish_mode.store(1, Ordering::Release);
        let mode = Arc::clone(&client.publish_mode);
        let publications = Arc::clone(&client.publications);
        let publish_timeouts = Arc::new(AtomicU64::new(0));
        let (shutdown_tx, shutdown) = watch::channel(false);
        let task = tokio::spawn(publisher_loop(
            Arc::new(client),
            Arc::from("comp-nested"),
            receiver,
            notifier,
            Arc::clone(&lost),
            Arc::clone(&publish_timeouts),
            shutdown,
        ));
        tokio::time::timeout(Duration::from_secs(1), async {
            while lost.load(Ordering::Acquire) != 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("failed publication drains the first backlog");

        mode.store(0, Ordering::Release);
        producer.offer(ObservationRecord::PropsChanged {
            path: "input.corners.enabled".into(),
            old: PropValue::Bool(true),
            new: PropValue::Bool(false),
            unix_ms: 0,
            cause: "props.set",
            event_seq: 3,
        });
        producer.offer(ObservationRecord::FocusChanged {
            keyboard: Some(4),
            previous: Some(2),
            exclusive_latch: None,
            event_seq: 4,
        });
        producer.offer(ObservationRecord::FocusChanged {
            keyboard: Some(5),
            previous: Some(4),
            exclusive_latch: None,
            event_seq: 5,
        });

        tokio::time::timeout(Duration::from_secs(1), async {
            while publications
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .len()
                < 4
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("publisher-loss and carried overflow gaps both publish");
        shutdown_tx.send_replace(true);
        task.await.expect("publisher exits");

        let published = publications
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let focus_gap = cosmix_bus::bus::parse(&published[0].1).expect("focus gap parses");
        assert_eq!(focus_gap.get("command"), Some("focus.changed"));
        assert_eq!(focus_gap.get("event_seq"), Some("2"));
        assert_eq!(
            serde_json::from_str::<Value>(&focus_gap.body).unwrap(),
            json!({"gap": true, "lost_count": 3, "cause": "publisher.loss"})
        );
        let first_survivor = cosmix_bus::bus::parse(&published[1].1).expect("survivor parses");
        assert_eq!(first_survivor.get("event_seq"), Some("4"));
        let second_survivor =
            cosmix_bus::bus::parse(&published[2].1).expect("second survivor parses");
        assert_eq!(second_survivor.get("event_seq"), Some("5"));
        let props_gap = cosmix_bus::bus::parse(&published[3].1).expect("props gap parses");
        assert_eq!(props_gap.get("command"), Some("props.changed"));
        assert_eq!(props_gap.get("event_seq"), Some("3"));
        assert_eq!(
            serde_json::from_str::<Value>(&props_gap.body).unwrap(),
            json!({"gap": true, "lost_count": 3, "cause": "outbox.overflow"})
        );
        assert_eq!(publish_timeouts.load(Ordering::Acquire), 1);
    }

    #[tokio::test]
    async fn rejected_gap_discards_its_survivor_and_recovers_as_publisher_loss() {
        let lost = Arc::new(AtomicU64::new(0));
        let (mut producer, receiver) = port_observation::outbox(Arc::clone(&lost));
        let notifier = producer.notifier();
        for sequence in 1..=3 {
            producer.offer(ObservationRecord::FocusChanged {
                keyboard: Some(sequence),
                previous: None,
                exclusive_latch: None,
                event_seq: sequence,
            });
        }
        let (client, _commands, _states) = FakeClient::new(ConnState::Connected, false);
        client.publish_mode.store(1, Ordering::Release);
        let mode = Arc::clone(&client.publish_mode);
        let publications = Arc::clone(&client.publications);
        let publish_timeouts = Arc::new(AtomicU64::new(0));
        let (shutdown_tx, shutdown) = watch::channel(false);
        let task = tokio::spawn(publisher_loop(
            Arc::new(client),
            Arc::from("comp-nested"),
            receiver,
            notifier,
            Arc::clone(&lost),
            Arc::clone(&publish_timeouts),
            shutdown,
        ));
        tokio::time::timeout(Duration::from_secs(1), async {
            while lost.load(Ordering::Acquire) != 3 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("failed gap discards every surviving record");
        mode.store(0, Ordering::Release);
        producer.offer(ObservationRecord::FocusChanged {
            keyboard: Some(4),
            previous: Some(3),
            exclusive_latch: None,
            event_seq: 4,
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            while publications
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .len()
                != 2
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("replacement gap and survivor publish");
        shutdown_tx.send_replace(true);
        task.await.expect("publisher exits");
        let published = publications
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let gap = cosmix_bus::bus::parse(&published[0].1).expect("gap parses");
        assert_eq!(gap.get("event_seq"), Some("3"));
        assert_eq!(
            serde_json::from_str::<Value>(&gap.body).unwrap(),
            json!({"gap": true, "lost_count": 3, "cause": "publisher.loss"})
        );
        assert_eq!(publish_timeouts.load(Ordering::Acquire), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_publication_times_out_without_blocking_shutdown() {
        let lost = Arc::new(AtomicU64::new(0));
        let (mut producer, receiver) = port_observation::outbox(Arc::clone(&lost));
        let notifier = producer.notifier();
        producer.offer(ObservationRecord::FocusChanged {
            keyboard: None,
            previous: Some(1),
            exclusive_latch: None,
            event_seq: 1,
        });
        let (client, _commands, _states) = FakeClient::new(ConnState::Connected, false);
        client.publish_mode.store(2, Ordering::Release);
        let publish_timeouts = Arc::new(AtomicU64::new(0));
        let (shutdown_tx, shutdown) = watch::channel(false);
        let task = tokio::spawn(publisher_loop(
            Arc::new(client),
            Arc::from("comp-nested"),
            receiver,
            notifier,
            Arc::clone(&lost),
            Arc::clone(&publish_timeouts),
            shutdown,
        ));
        tokio::task::yield_now().await;
        tokio::time::advance(PUBLISH_TIMEOUT).await;
        tokio::task::yield_now().await;
        assert_eq!(publish_timeouts.load(Ordering::Acquire), 1);
        assert_eq!(lost.load(Ordering::Acquire), 1);
        shutdown_tx.send_replace(true);
        tokio::time::timeout(Duration::from_millis(1), task)
            .await
            .expect("publisher shutdown is bounded")
            .expect("publisher exits");
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_publisher_does_not_block_commands_and_worker_shutdown_is_bounded() {
        let (ingress, _source, _) = test_ingress();
        let broker = Arc::new(AtomicU8::new(BROKER_RETRYING));
        let reply_timeouts = Arc::new(AtomicU64::new(0));
        let publish_timeouts = Arc::new(AtomicU64::new(0));
        let lost = Arc::new(AtomicU64::new(0));
        let (mut producer, observations) = port_observation::outbox(Arc::clone(&lost));
        let notifier = producer.notifier();
        producer.offer(ObservationRecord::FocusChanged {
            keyboard: Some(1),
            previous: None,
            exclusive_latch: None,
            event_seq: 1,
        });
        let (client, commands, _states) = FakeClient::new(ConnState::Connected, false);
        client.publish_mode.store(2, Ordering::Release);
        let responses_started = Arc::clone(&client.responses_started);
        let mut client = Some(client);
        let (shutdown_tx, shutdown) = watch::channel(false);
        let worker = tokio::spawn(worker_loop(
            "comp-nested".into(),
            ingress,
            Arc::clone(&broker),
            reply_timeouts,
            publish_timeouts,
            observations,
            notifier,
            lost,
            shutdown,
            move || future::ready(Ok(client.take().expect("one connection attempt"))),
        ));
        wait_for_broker(&broker, BROKER_CONNECTED).await;
        commands.send(command("comp.ping", 1)).expect("worker live");
        wait_for_counter(&responses_started, 1, "ping replied while publish hangs").await;
        shutdown_tx.send_replace(true);
        tokio::time::timeout(Duration::from_millis(1), worker)
            .await
            .expect("worker shutdown does not await publisher deadline")
            .expect("worker exits");
    }

    #[tokio::test]
    async fn bounded_ingress_releases_depth_through_production_admission_completion() {
        let (ingress, source, queue_depth) = test_ingress();
        let mut admissions = Vec::new();
        for _ in 0..PORT_QUEUE_CAPACITY {
            admissions.push(ingress.request_snapshot().expect("request admitted"));
        }
        assert!(ingress.request_snapshot().is_err());
        assert_eq!(queue_depth.load(Ordering::Acquire), PORT_QUEUE_CAPACITY);

        for _ in 0..PORT_QUEUE_CAPACITY {
            let PortCommand::Snapshot(request) = source.try_recv().expect("staged request") else {
                panic!("snapshot request expected");
            };
            drop(request);
        }
        for admission in admissions {
            assert!(admission.receive().await.is_err());
        }
        assert_eq!(queue_depth.load(Ordering::Acquire), 0);
    }

    #[tokio::test]
    async fn completed_responders_are_reaped_and_seventeenth_command_is_admitted() {
        let (ingress, source, queue_depth) = test_ingress();
        let broker = Arc::new(AtomicU8::new(BROKER_RETRYING));
        let reply_timeouts = Arc::new(AtomicU64::new(0));
        let (shutdown_tx, shutdown) = watch::channel(false);
        let (client, commands, _states) = FakeClient::new(ConnState::Connected, false);
        let mut client = Some(client);
        let (publish_timeouts, observations, lost_count) = test_observation_args();
        let worker = tokio::spawn(worker_loop(
            "comp-nested".into(),
            ingress,
            Arc::clone(&broker),
            reply_timeouts,
            publish_timeouts,
            observations,
            Arc::new(tokio::sync::Notify::new()),
            lost_count,
            shutdown,
            move || future::ready(Ok(client.take().expect("one connection attempt"))),
        ));
        wait_for_broker(&broker, BROKER_CONNECTED).await;

        for id in 0..PORT_QUEUE_CAPACITY {
            commands
                .send(command("comp.info", id))
                .expect("worker live");
        }
        for _ in 0..PORT_QUEUE_CAPACITY {
            let PortCommand::Snapshot(request) = next_port_command(&source).await else {
                panic!("snapshot request expected");
            };
            drop(request);
        }
        tokio::time::timeout(Duration::from_secs(1), async {
            while queue_depth.load(Ordering::Acquire) != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("production responder completion releases all admissions");

        for id in 100..164 {
            commands
                .send(command("comp.ping", id))
                .expect("worker live");
        }
        commands
            .send(command("comp.info", PORT_QUEUE_CAPACITY))
            .expect("worker live");
        let PortCommand::Snapshot(request) = next_port_command(&source).await else {
            panic!("snapshot request expected");
        };
        drop(request);

        shutdown_tx.send_replace(true);
        worker.await.expect("worker exits cleanly");
    }

    #[tokio::test(start_paused = true)]
    async fn worker_loop_abandons_black_holed_replies_and_stays_responsive() {
        let (ingress, source, queue_depth) = test_ingress();
        let broker = Arc::new(AtomicU8::new(BROKER_RETRYING));
        let reply_timeouts = Arc::new(AtomicU64::new(0));
        let (shutdown_tx, shutdown) = watch::channel(false);
        let (client, commands, _states) = FakeClient::new(ConnState::Connected, true);
        let responses_started = Arc::clone(&client.responses_started);
        let mut client = Some(client);
        let (publish_timeouts, observations, lost_count) = test_observation_args();
        let worker = tokio::spawn(worker_loop(
            "comp-nested".into(),
            ingress,
            Arc::clone(&broker),
            Arc::clone(&reply_timeouts),
            publish_timeouts,
            observations,
            Arc::new(tokio::sync::Notify::new()),
            lost_count,
            shutdown,
            move || future::ready(Ok(client.take().expect("one connection attempt"))),
        ));
        wait_for_broker(&broker, BROKER_CONNECTED).await;

        commands
            .send(command("comp.ping", 1))
            .expect("worker admits ping");
        commands
            .send(command("comp.unknown", 2))
            .expect("worker admits error reply");
        wait_for_counter(&responses_started, 1, "first reply send starts").await;
        assert_eq!(queue_depth.load(Ordering::Acquire), 0);

        tokio::time::advance(REPLY_SEND_TIMEOUT).await;
        wait_for_counter(&responses_started, 2, "second reply send starts").await;
        assert_eq!(reply_timeouts.load(Ordering::Acquire), 1);
        tokio::time::advance(REPLY_SEND_TIMEOUT).await;
        tokio::task::yield_now().await;
        assert_eq!(reply_timeouts.load(Ordering::Acquire), 2);
        assert_eq!(queue_depth.load(Ordering::Acquire), 0);

        commands
            .send(command("comp.info", 3))
            .expect("worker remains responsive");
        let PortCommand::Snapshot(request) = next_port_command(&source).await else {
            panic!("snapshot request expected");
        };
        assert_eq!(queue_depth.load(Ordering::Acquire), 1);
        drop(request);
        wait_for_counter(&queue_depth, 0, "later admission releases depth").await;
        wait_for_counter(&responses_started, 3, "later error reply send starts").await;
        tokio::time::advance(REPLY_SEND_TIMEOUT).await;
        tokio::task::yield_now().await;
        assert_eq!(reply_timeouts.load(Ordering::Acquire), 3);

        shutdown_tx.send_replace(true);
        worker.await.expect("worker exits cleanly");
    }

    #[tokio::test(start_paused = true)]
    async fn reply_sender_abandons_black_holed_reply_after_deadline_and_counts_it() {
        let (client, _commands, _states) = FakeClient::new(ConnState::Connected, true);
        let client = Arc::new(client);
        let reply_timeouts = Arc::new(AtomicU64::new(0));
        let (sender, receiver) = tokio_mpsc::channel(1);
        let task = tokio::spawn(reply_loop(
            client,
            Arc::from("comp-nested"),
            receiver,
            Arc::clone(&reply_timeouts),
        ));
        sender
            .send(PendingReply::new(
                command("comp.ping", 1),
                (0, Arc::from("{}")),
            ))
            .await
            .expect("reply lane open");
        tokio::task::yield_now().await;
        tokio::time::advance(REPLY_SEND_TIMEOUT).await;
        tokio::task::yield_now().await;
        assert_eq!(reply_timeouts.load(Ordering::Acquire), 1);
        drop(sender);
        task.await.expect("reply sender exits");
    }

    #[tokio::test]
    async fn graceful_shutdown_deregisters_then_closes_when_broker_answers() {
        let (client, _commands, _states) = FakeClient::new(ConnState::Connected, false);
        let deregistered = Arc::clone(&client.deregistered);
        let closed = Arc::clone(&client.closed);

        graceful_client_shutdown(&client).await;

        assert_eq!(deregistered.load(Ordering::Acquire), 1);
        assert_eq!(closed.load(Ordering::Acquire), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn graceful_shutdown_closes_within_budget_when_deregister_hangs() {
        let (client, _commands, _states) = FakeClient::new(ConnState::Connected, false);
        client.deregister_hangs.store(true, Ordering::Release);
        let deregistered = Arc::clone(&client.deregistered);
        let closed = Arc::clone(&client.closed);
        let shutdown = tokio::spawn(async move {
            graceful_client_shutdown(&client).await;
        });
        tokio::task::yield_now().await;
        tokio::time::advance(CLIENT_SHUTDOWN_BUDGET).await;

        shutdown.await.expect("bounded shutdown completes");
        assert_eq!(deregistered.load(Ordering::Acquire), 1);
        assert_eq!(closed.load(Ordering::Acquire), 1);
    }
}
