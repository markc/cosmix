//! Recipient-owned BROKER-023 policy. Transport stamps enter only through
//! VerifiedCommand; request bodies never construct an actor.
use crate::tabs::{Cleanup, TabSet};
use cosmix_bus::native_session::*;
use cosmix_client::session::{Deadline, Hello};
use cosmix_client::{VerifiedCommand, VerifiedConnection};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const RETENTION: Duration = Duration::from_secs(900);
/// BROKER-020's renew cadence. A cached bound-caller check is reused no longer
/// than one such window, so a revocation missed by every notice still closes
/// the lane within the lease it was granted under.
const LEASE_WINDOW: Duration = Duration::from_secs(5);
const PER_ACTOR: usize = 1024;
const TOTAL: usize = 4096;
const ACTORS: usize = 256;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Target {
    pub instance_id: HexBytes<16>,
    pub incarnation: HexBytes<16>,
    pub pane_id: DecimalU64,
    pub pane_generation: DecimalU64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    target: Target,
    #[serde(default)]
    affected: Vec<Target>,
    #[serde(default)]
    request_id: Option<DecimalU64>,
    #[serde(default)]
    operation_id: Option<DecimalU64>,
    #[serde(default)]
    request_epoch: Option<HexBytes<16>>,
    #[serde(default)]
    foreground_generation: Option<DecimalU64>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    dir: Option<String>,
    #[serde(default)]
    contents: bool,
    #[serde(default)]
    property: Option<String>,
    #[serde(default)]
    value: Option<Value>,
}

#[derive(Clone)]
pub struct Reply {
    pub rc: u8,
    pub body: String,
}

#[derive(Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum FailureCode {
    InvalidArgument,
    NotFound,
    StaleGeneration,
    Conflict,
    Busy,
    Forbidden,
    Unsupported,
    ResourceLimit,
    Disconnected,
    Expired,
    Cancelled,
    UnknownOutcome,
}
#[derive(Serialize)]
struct Failure {
    error_code: FailureCode,
}
impl Reply {
    fn ok(value: Value) -> Self {
        Self {
            rc: 0,
            body: value.to_string(),
        }
    }
    pub fn error(code: &'static str) -> Self {
        let error_code = match code {
            "INVALID_ARGUMENT" => FailureCode::InvalidArgument,
            "NOT_FOUND" => FailureCode::NotFound,
            "STALE_GENERATION" => FailureCode::StaleGeneration,
            "CONFLICT" => FailureCode::Conflict,
            "BUSY" => FailureCode::Busy,
            "FORBIDDEN" => FailureCode::Forbidden,
            "UNSUPPORTED" => FailureCode::Unsupported,
            "RESOURCE_LIMIT" => FailureCode::ResourceLimit,
            "DISCONNECTED" => FailureCode::Disconnected,
            "EXPIRED" => FailureCode::Expired,
            "CANCELLED" => FailureCode::Cancelled,
            "UNKNOWN_OUTCOME" => FailureCode::UnknownOutcome,
            _ => unreachable!("unregistered Term error token"),
        };
        Self {
            rc: 10,
            body: serde_json::to_string(&Failure { error_code }).expect("owned failure schema"),
        }
    }
}

/// A permit is checked again at the actual PTY write boundary. Lifecycle
/// invalidation and human keys close it without waiting for Bus work.
pub struct Permit {
    pub connection: Arc<dyn Fn() -> bool + Send + Sync>,
    pub live: std::sync::atomic::AtomicBool,
    pub parent: Deadline,
    pub actor: Option<Deadline>,
    pub hello: Hello,
    pub until: Instant,
    pub pane: Arc<dyn Fn() -> bool + Send + Sync>,
    pub written: std::sync::atomic::AtomicU64,
    notice: Mutex<Option<(tokio::sync::mpsc::Sender<InputNotice>, InputNotice)>>,
    /// Woken the instant a revocation queues a notice, so the actor drains on
    /// the event rather than on a clock.
    wake: Arc<tokio::sync::Notify>,
}
impl Permit {
    pub fn valid(&self) -> bool {
        self.live.load(std::sync::atomic::Ordering::Acquire)
            && (self.connection)()
            && Instant::now() < self.until
            && self.parent.is_live(&self.hello).unwrap_or(false)
            && self
                .actor
                .as_ref()
                .is_none_or(|a| a.is_live(&self.hello).unwrap_or(false))
            && (self.pane)()
    }
    pub fn revoke(&self) {
        if self.live.swap(false, std::sync::atomic::Ordering::AcqRel)
            && let Some((sender, mut notice)) = self.notice.lock().unwrap().take()
        {
            notice.written = self.written.load(std::sync::atomic::Ordering::Acquire);
            if sender.try_send(notice).is_ok() {
                // Revocation is the event; the actor never polls for it. This
                // runs on the render thread under the write lock, so it must
                // stay a bare wakeup and do no Bus work of its own.
                self.wake.notify_one();
            }
        }
    }
}

struct Entry {
    target: Target,
    outcome: Option<Reply>,
    sequence: u64,
    digest: [u8; 32],
    at: Instant,
    reply: Reply,
}
struct History {
    high_water: u64,
    entries: VecDeque<Entry>,
    /// Last accepted mutation. Only used to pick the coldest key to evict at
    /// the actor cap; it never affects whether a retry is answered.
    last: Instant,
}
impl Default for History {
    fn default() -> Self {
        Self {
            high_water: 0,
            entries: VecDeque::new(),
            last: Instant::now(),
        }
    }
}
#[derive(Default)]
struct State {
    history: HashMap<String, History>,
    permits: Vec<(Option<RecordRef>, std::sync::Weak<Permit>)>,
    /// Bound-caller lease checks, reused within one lease window. Lifecycle
    /// notices and gaps drop these with the permits they authorised, so a
    /// cached check can never outlive the authority it recorded. A list, not a
    /// map: `RecordRef` is a wire type without `Hash`, and this holds at most
    /// one entry per live bound child.
    leases: Vec<(RecordRef, Instant, Deadline)>,
}

struct InputNotice {
    actor: String,
    expected: usize,
    to: String,
    actor_epoch: HexBytes<16>,
    actor_connection: HexBytes<16>,
    target: Target,
    request_id: Option<DecimalU64>,
    written: u64,
}

pub struct Control {
    terminal: Arc<Mutex<TabSet>>,
    cleanup: Cleanup,
    state: Mutex<State>,
    native: crate::native_session::NativeSession,
    notice_tx: tokio::sync::mpsc::Sender<InputNotice>,
    notice_rx: Mutex<tokio::sync::mpsc::Receiver<InputNotice>>,
    wake: Arc<tokio::sync::Notify>,
}
impl Control {
    #[cfg(test)]
    pub(crate) fn expire_retries(&self) {
        for history in self.state.lock().unwrap().history.values_mut() {
            for entry in &mut history.entries {
                entry.at = Instant::now() - RETENTION;
            }
        }
    }
    pub fn new(
        terminal: Arc<Mutex<TabSet>>,
        cleanup: Cleanup,
        native: crate::native_session::NativeSession,
    ) -> Self {
        let (notice_tx, notice_rx) = tokio::sync::mpsc::channel(256);
        Self {
            terminal,
            cleanup,
            native,
            state: Mutex::new(State::default()),
            notice_tx,
            notice_rx: Mutex::new(notice_rx),
            wake: Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// The actor waits on this instead of ticking. Revocation queues a notice
    /// and wakes it; a permit that merely aged out is collected on the next
    /// wake or renew, since nothing is owed to anyone until one is queued.
    pub fn wake(&self) -> Arc<tokio::sync::Notify> {
        self.wake.clone()
    }

    pub async fn flush_notices(&self, connection: &VerifiedConnection) {
        {
            let mut state = self.state.lock().unwrap();
            state.permits.retain(|(_, weak)| {
                let Some(permit) = weak.upgrade() else {
                    return false;
                };
                if !permit.valid() {
                    permit.revoke();
                    return false;
                }
                true
            });
        }
        // Bound work per actor turn as well as retained queue memory. Name
        // reuse cannot redirect these private events: noded atomically checks
        // the original epoch/connection at the actual delivery boundary.
        for _ in 0..16 {
            let Some(notice) = self.notice_rx.lock().unwrap().try_recv().ok() else {
                break;
            };
            // The lease ended; whether the write finished first is a separate
            // question, answered by the byte count. The event and the retained
            // outcome MUST agree on it — a caller that reads one and a caller
            // that reads the other are asking the same thing.
            let completed = notice.written >= notice.expected as u64;
            let status = if completed { "completed" } else { "unknown" };
            let outcome = if completed { "complete" } else { "partial_or_unknown" };
            {
                let mut state = self.state.lock().unwrap();
                if let Some(entry) = state.history.get_mut(&notice.actor).and_then(|h| {
                    h.entries
                        .iter_mut()
                        .find(|e| Some(DecimalU64(e.sequence)) == notice.request_id)
                }) {
                    entry.outcome = Some(Reply::ok(json!({
                        "operation_id":notice.request_id,
                        "status":status,
                        "boundary":"pty_write", "reason":"input_lease_ended",
                        "delivered_bytes_lower_bound":notice.written,
                    })));
                }
            }
            let message = cosmix_bus::bus::BusMessage::new()
                .with_header("from", &connection.client().name())
                .with_header("to", &notice.to)
                .with_header("type", "event")
                .with_header("command", "term.input.revoked")
                .with_header("recipient_connection", &json!({"broker_epoch":notice.actor_epoch,"connection_id":notice.actor_connection}).to_string())
                .with_body(&json!({"target":notice.target,"request_id":notice.request_id,"status":"revoked","outcome":outcome,"delivered_bytes_lower_bound":notice.written}).to_string());
            if tokio::time::timeout(
                Duration::from_millis(100),
                connection.client().send_raw(&message),
            )
            .await
            .is_err()
            {
                break;
            }
        }
    }

    pub fn invalidate(&self, target: Option<&RecordRef>) {
        let mut state = self.state.lock().unwrap();
        // Drop the cached checks first: a notice or gap means the authority
        // they recorded is no longer current, and the next request must pay
        // for a fresh one rather than resolve against a stale window.
        match target {
            None => state.leases.clear(),
            Some(t) => state.leases.retain(|(reference, _, _)| {
                reference.record_id != t.record_id
                    || reference.incarnation != t.incarnation
                    || reference.binding_generation.0 > t.binding_generation.0
            }),
        }
        state.permits.retain(|(actor, weak)| {
            let Some(permit) = weak.upgrade() else {
                return false;
            };
            if target.is_none_or(|t| {
                [&Some(permit.parent.target().clone()), actor]
                    .into_iter()
                    .any(|r| {
                        r.as_ref().is_some_and(|r| {
                            r.record_id == t.record_id
                                && r.incarnation == t.incarnation
                                && r.binding_generation.0 <= t.binding_generation.0
                        })
                    })
            }) {
                permit.revoke();
            }
            permit.valid()
        });
    }

    pub async fn dispatch(
        &self,
        connection: &VerifiedConnection,
        parent: &SessionRecord,
        own: &(Hello, Deadline),
        event: &VerifiedCommand,
    ) -> Reply {
        let native = &self.native;
        if !connection.client().is_connected() {
            self.invalidate(None);
            return Reply::error("DISCONNECTED");
        }
        let Some(actor) = event.trusted_context() else {
            return Reply::error("FORBIDDEN");
        };
        // Deny before argument parsing: untrusted callers cannot probe schema,
        // target existence, policy or supported operation details.
        if !principal_allowed(parent, actor) {
            return Reply::error("FORBIDDEN");
        }
        let command = event.command();
        if command.body.len() > 8192 {
            return Reply::error("INVALID_ARGUMENT");
        }
        let Ok(mut request) = serde_json::from_str::<Request>(&command.body) else {
            return Reply::error("INVALID_ARGUMENT");
        };
        // Live properties adapt to the same owner operations and commitment
        // lock, rather than storing a second writable copy of pane state.
        let property = command.command.starts_with("term.props.");
        let verb = match (command.command.as_str(), request.property.as_deref()) {
            ("term.props.get", Some("state")) => "term.session",
            ("term.props.get", Some("contents")) => {
                request.contents = true;
                "term.snapshot"
            }
            ("term.props.set", Some("selected")) if request.value == Some(Value::Bool(true)) => {
                "term.pane.select"
            }
            ("term.props.set", Some("input")) => {
                request.text = request
                    .value
                    .as_ref()
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                "term.type"
            }
            (verb, _) if !property => verb,
            _ => return Reply::error("UNSUPPORTED"),
        };
        let capability = match verb {
            "term.session" | "term.list" | "term.tabs" | "term.panes" | "term.operation" => {
                Capability::ReadState
            }
            "term.snapshot" if !request.contents => Capability::ReadState,
            "term.snapshot" => Capability::ReadContents,
            "term.type" => Capability::Input,
            "term.tab.new" | "term.tab.select" | "term.pane.split" | "term.pane.select" => {
                Capability::ManageLayout
            }
            "term.tab.close" | "term.pane.close" => Capability::Terminate,
            "term.execute" => Capability::Execute,
            _ => return Reply::error("UNSUPPORTED"),
        };
        if (request.request_id.is_some() || request.operation_id.is_some())
            && actor.session.is_none()
            && (request.target.instance_id != parent.instance_id
                || request.target.incarnation != parent.incarnation)
        {
            return Reply::error("UNKNOWN_OUTCOME");
        }
        if !allows(parent, actor, &request.target, capability) {
            return Reply::error("FORBIDDEN");
        }
        if matches!(verb, "term.tab.new" | "term.pane.split")
            && actor
                .session
                .as_ref()
                .is_some_and(|s| s.role == Role::PaneShell)
        {
            return Reply::error("FORBIDDEN");
        }
        // Deliberately after the capability decision, not before it: a caller
        // without execute authority learns FORBIDDEN like any other refusal and
        // is told nothing about what the verb would have done. Only a caller
        // who would have been allowed sees that the verb is unimplemented,
        // which is the documented stage-D state rather than a leak.
        if capability == Capability::Execute {
            return Reply::error("UNSUPPORTED");
        }
        // The recipient's own deadline comes from its renew cadence, never
        // from a check made here: `lease.check` answers only for a record this
        // connection holds a delivery dependency on (PROP-025), which a
        // recipient never holds on its own attachment. A failed or stale renew
        // leaves no deadline at all, so an otherwise ambient owner loses
        // protected access the moment Term's own attachment does.
        let (hello, parent_deadline) = (own.0.clone(), own.1.clone());
        if parent_deadline.target() != &parent.reference()
            || !parent_deadline.is_live(&hello).unwrap_or(false)
        {
            return Reply::error("FORBIDDEN");
        }
        // A bound caller's own lease is checked against the broker. Its request
        // registered the dependency this needs, so the check is answerable, and
        // it completes here — outside the synchronous capability resolution
        // below, which may never block on a Bus call (PROP-024). A live cached
        // check from this lease window satisfies the requirement instead.
        let actor_deadline = if let Some(s) = &actor.session {
            let reference = reference(s);
            let cached = self
                .state
                .lock()
                .unwrap()
                .leases
                .iter()
                .find(|(target, at, deadline)| {
                    *target == reference
                        && at.elapsed() < LEASE_WINDOW
                        && deadline.is_live(&hello).unwrap_or(false)
                })
                .map(|(_, _, deadline)| deadline.clone());
            let deadline = match cached {
                Some(deadline) => deadline,
                None => {
                    let fresh = tokio::time::timeout(
                        Duration::from_secs(2),
                        connection.session_lease_check(reference.clone()),
                    )
                    .await;
                    let Ok(Ok(deadline)) = fresh else {
                        return Reply::error("FORBIDDEN");
                    };
                    let mut state = self.state.lock().unwrap();
                    state.leases.retain(|(target, at, _)| {
                        *target != reference && at.elapsed() < LEASE_WINDOW
                    });
                    if state.leases.len() < ACTORS {
                        state
                            .leases
                            .push((reference, Instant::now(), deadline.clone()));
                    }
                    deadline
                }
            };
            if !deadline.is_live(&hello).unwrap_or(false) {
                return Reply::error("FORBIDDEN");
            }
            Some(deadline)
        } else {
            None
        };
        // A completed close can be retried after removal of its pane. Resolve
        // retained results after actor/recipient lease checks but before live
        // target lookup. Retired IDs never re-enter the mutation path.
        let identity = actor_key(actor);
        let digest: [u8; 32] =
            Sha256::digest(format!("{}\0{}", command.command, command.body).as_bytes()).into();
        if verb == "term.operation" {
            let Some(id) = request.operation_id else {
                return Reply::error("INVALID_ARGUMENT");
            };
            let state = self.state.lock().unwrap();
            return match state.history.get(&identity).and_then(|h| {
                h.entries
                    .iter()
                    .find(|e| e.sequence == id.0 && e.at.elapsed() < RETENTION)
            }) {
                Some(entry) if entry.target == request.target => {
                    entry.outcome.as_ref().unwrap_or(&entry.reply).clone()
                }
                Some(_) => Reply::error("FORBIDDEN"),
                None => Reply::error("UNKNOWN_OUTCOME"),
            };
        }
        if let Some(sequence) = request.request_id {
            let mut state = self.state.lock().unwrap();
            for history in state.history.values_mut() {
                while history
                    .entries
                    .front()
                    .is_some_and(|e| e.at.elapsed() >= RETENTION)
                {
                    history.entries.pop_front();
                }
            }
            if let Some(history) = state.history.get(&identity) {
                if let Some(entry) = history.entries.iter().find(|e| e.sequence == sequence.0) {
                    return if entry.digest == digest {
                        entry.reply.clone()
                    } else {
                        Reply::error("CONFLICT")
                    };
                }
                if sequence.0 <= history.high_water {
                    return Reply::error("UNKNOWN_OUTCOME");
                }
            }
        }
        let target = request.target.clone();
        let live = native.pane_guard(target.pane_id.0, target.pane_generation.0);
        let permit = Arc::new(Permit {
            connection: Arc::new(connection.client().connection_liveness()),
            live: std::sync::atomic::AtomicBool::new(true),
            parent: parent_deadline,
            actor: actor_deadline,
            hello,
            until: Instant::now() + Duration::from_secs(2),
            pane: live,
            written: std::sync::atomic::AtomicU64::new(0),
            notice: Mutex::new(None),
            wake: self.wake.clone(),
        });
        let mut tabs = self.terminal.lock().unwrap();
        if !permit.valid() || tabs.pane_by_id(target.pane_id.0).is_none() {
            return Reply::error("FORBIDDEN");
        }
        // `term.props.*` is a VERB ALIAS, not a second authorisation surface.
        // Each property name resolved to a verb above and everything after this
        // point is the enforcement for both spellings, which is precisely what
        // PROP-024 requires: the property owner shares BROKER-023's policy and
        // current-target checks with its verb handlers.
        //
        // There used to be an AuthPolicy here that rebuilt a PeerIdentity from
        // the actor this function had already authorised, then asked `allows`
        // the same question again with the same arguments. It could not fail on
        // any reachable path, and it read like an independent gate — which is
        // worse than no gate, because a reader counts it as defence. If a
        // second surface is ever wanted it has to consult something `allows`
        // does not; until then the alias carries no separate check.
        let layout = matches!(capability, Capability::ManageLayout | Capability::Terminate);
        if layout {
            let affected = tabs.control_affected(target.pane_id.0, verb);
            if affected.is_empty()
                || affected.iter().any(|id| {
                    let t = if *id == target.pane_id.0 {
                        Some(&target)
                    } else {
                        request.affected.iter().find(|t| t.pane_id.0 == *id)
                    };
                    t.is_none_or(|t| {
                        !allows(parent, actor, t, Capability::ManageLayout)
                            || !native.pane_guard(t.pane_id.0, t.pane_generation.0)()
                    })
                })
            {
                return Reply::error("FORBIDDEN");
            }
            if verb == "term.tab.close"
                && affected
                    .iter()
                    .filter(|id| tabs.control_tab(**id) == tabs.control_tab(target.pane_id.0))
                    .any(|id| {
                        let t = if *id == target.pane_id.0 {
                            &target
                        } else {
                            request
                                .affected
                                .iter()
                                .find(|t| t.pane_id.0 == *id)
                                .unwrap()
                        };
                        !allows(parent, actor, t, Capability::Terminate)
                    })
            {
                return Reply::error("FORBIDDEN");
            }
        }
        let mutation = layout || capability == Capability::Input;
        if mutation {
            match request.request_epoch {
                None => return Reply::error("INVALID_ARGUMENT"),
                Some(epoch) if epoch != actor.connection_id => {
                    return Reply::error("UNKNOWN_OUTCOME");
                }
                Some(_) => {}
            }
        }
        let mut state = self.state.lock().unwrap();
        if mutation {
            let Some(sequence) = request.request_id.map(|id| id.0).filter(|id| *id > 0) else {
                return Reply::error("INVALID_ARGUMENT");
            };
            for history in state.history.values_mut() {
                while history
                    .entries
                    .front()
                    .is_some_and(|e| e.at.elapsed() >= RETENTION)
                {
                    history.entries.pop_front();
                }
            }
            let total = state
                .history
                .values()
                .map(|h| h.entries.len())
                .sum::<usize>();
            // A key keeps its high-water mark even after every entry expires,
            // so a late retry answers UNKNOWN_OUTCOME instead of re-executing;
            // ageing alone therefore never drops one. The cap still has to
            // evict rather than refuse. An ambient key names one connection and
            // a connection id never returns, so a full table is overwhelmingly
            // keys that can never be addressed again — refusing at the cap
            // would brick every mutation on the instance permanently, which is
            // strictly worse than losing the coldest actor's dedupe. Keys with
            // no live entry go first, since losing one costs only a high-water
            // mark; then the coldest overall. BROKER-023 allows earlier
            // eviction at the instance cap, and TOTAL still bounds memory.
            if !state.history.contains_key(&identity) && state.history.len() >= ACTORS {
                let victim = state
                    .history
                    .iter()
                    .min_by_key(|(_, history)| (!history.entries.is_empty(), history.last))
                    .map(|(key, _)| key.clone());
                if let Some(key) = victim {
                    state.history.remove(&key);
                }
            }
            let history = state.history.entry(identity.clone()).or_default();
            if let Some(entry) = history.entries.iter().find(|e| e.sequence == sequence) {
                return if entry.digest == digest {
                    entry.reply.clone()
                } else {
                    Reply::error("CONFLICT")
                };
            }
            if sequence <= history.high_water {
                return Reply::error("UNKNOWN_OUTCOME");
            }
            if total >= TOTAL {
                return Reply::error("RESOURCE_LIMIT");
            }
            history.high_water = sequence;
        }
        if !permit.valid() {
            return Reply::error("FORBIDDEN");
        }
        let mut reply = match verb {
            "term.session" | "term.list" | "term.tabs" | "term.panes" => {
                let panes: Vec<_> = tabs.control_panes().into_iter().filter_map(|p| {
                    let generation = native.pane_generation(p.id)?;
                    let t = Target { pane_id:DecimalU64(p.id), pane_generation:DecimalU64(generation), ..target.clone() };
                    allows(parent, actor, &t, Capability::ReadState).then(|| json!({
                        "target":t,"tab_id":tabs.control_tab(p.id),"cols":p.cols,"rows":p.rows,"child_pid":p.child_pid
                    }))
                }).collect();
                let tab_metadata: Vec<_> = tabs.list().into_iter().filter(|tab| {
                    tabs.control_panes().iter().filter(|p| tabs.control_tab(p.id) == Some(tab.id)).all(|p| {
                        native.pane_generation(p.id).is_some_and(|generation| {
                            allows(parent, actor, &Target { pane_id:DecimalU64(p.id), pane_generation:DecimalU64(generation), ..target.clone() }, Capability::ReadState)
                        })
                    })
                }).map(|tab| json!({"id":tab.id,"active":tab.active,"title":tab.title,"cols":tab.cols,"rows":tab.rows,"child_pid":tab.child_pid})).collect();
                Reply::ok(json!({
                    "instance_id":parent.instance_id, "incarnation":parent.incarnation,
                    "target":target, "policy":parent.policy,
                    "request_epoch":actor.connection_id,
                    "request_high_water":DecimalU64(state.history.get(&identity).map_or(0, |h| h.high_water)),
                    "panes":panes,
                    "tabs":tab_metadata,
                    "binding":tabs.session_status()["panes"][target.pane_id.0.to_string()],
                    "foreground_generation":tabs.pane_by_id(target.pane_id.0).unwrap().lock().unwrap().listener.foreground_generation(),
                    "retention_seconds":900, "requests_per_actor":PER_ACTOR,
                    "instance_request_limit":TOTAL,
                }))
            }
            "term.snapshot" => {
                let pane = tabs.pane_by_id(target.pane_id.0).unwrap();
                let pane = pane.lock().unwrap();
                if !permit.valid() {
                    return Reply::error("FORBIDDEN");
                }
                if request.contents {
                    Reply::ok(json!({"target":target,"text":pane.snapshot()}))
                } else {
                    Reply::ok(
                        json!({"target":target,"foreground_generation":pane.listener.foreground_generation()}),
                    )
                }
            }
            "term.type" => {
                let pane = tabs.pane_by_id(target.pane_id.0).unwrap();
                let pane = pane.lock().unwrap();
                match (request.text.as_deref(), request.foreground_generation) {
                    (Some(text), Some(generation)) => {
                        *permit.notice.lock().unwrap() = Some((
                            self.notice_tx.clone(),
                            InputNotice {
                                actor: identity.clone(),
                                expected: text.len(),
                                to: command.from.clone(),
                                actor_epoch: actor.broker_epoch,
                                actor_connection: actor.connection_id,
                                target: target.clone(),
                                request_id: request.request_id,
                                written: 0,
                            },
                        ));
                        match pane.listener.control_text(
                            text,
                            generation.0,
                            &identity,
                            permit.clone(),
                        ) {
                            Ok(()) => {
                                state.permits.retain(|(_, p)| p.strong_count() > 0);
                                state.permits.push((
                                    actor.session.as_ref().map(reference),
                                    Arc::downgrade(&permit),
                                ));
                                Reply::ok(
                                    json!({"status":"accepted","outcome":"unknown","request_id":request.request_id}),
                                )
                            }
                            Err(code) => Reply::error(code),
                        }
                    }
                    _ => Reply::error("INVALID_ARGUMENT"),
                }
            }
            _ => self.layout(&mut tabs, verb, &request),
        };
        if reply.body.len() > 256 * 1024 {
            if !mutation {
                return Reply::error("RESOURCE_LIMIT");
            }
            // A mutation has already committed by here; only its reply is
            // undeliverable, so reporting a plain limit failure would misstate
            // what happened. The high-water mark set before execution already
            // refuses re-execution, and recording this keeps a retry and
            // term.operation answering the same unknown outcome rather than
            // one of them finding no entry at all.
            reply = Reply::error("UNKNOWN_OUTCOME");
        }
        if mutation {
            if reply.rc == 0 {
                let mut result: Value =
                    serde_json::from_str(&reply.body).expect("owned reply JSON");
                result["operation_id"] = json!(request.request_id);
                reply.body = result.to_string();
            }
            let history = state.history.get_mut(&identity).unwrap();
            history.last = Instant::now();
            history.entries.push_back(Entry {
                target: request.target.clone(),
                outcome: None,
                sequence: request.request_id.unwrap().0,
                digest,
                at: Instant::now(),
                reply: reply.clone(),
            });
            if history.entries.len() > PER_ACTOR {
                history.entries.pop_front();
            }
        }
        reply
    }

    fn layout(&self, tabs: &mut TabSet, verb: &str, request: &Request) -> Reply {
        let id = request.target.pane_id.0;
        let Some(tab) = tabs.control_tab(id) else {
            return Reply::error("FORBIDDEN");
        };
        match verb {
            "term.tab.new" => match tabs.open() {
                Ok(id) => Reply::ok(json!({"tab_id":id})),
                Err(_) => Reply::error("RESOURCE_LIMIT"),
            },
            "term.tab.select" | "term.pane.select" => {
                if !tabs.select(tab) || !tabs.focus(id) {
                    return Reply::error("FORBIDDEN");
                }
                Reply::ok(json!({"selected":request.target}))
            }
            "term.tab.close" => {
                let (_, removed) = tabs.close(tab);
                self.cleanup.submit(removed.into_iter().collect());
                Reply::ok(json!({"closed":request.target}))
            }
            "term.pane.close" => {
                // close_active and split_active act on whatever is focused, so a
                // refused focus would silently retarget them at another pane.
                // BROKER-023 refuses implicit active-pane selection; that has
                // to be enforced, not left to the focus call happening to work.
                if !tabs.select(tab) || !tabs.focus(id) {
                    return Reply::error("FORBIDDEN");
                }
                let (_, removed) = tabs.close_active();
                self.cleanup.submit(removed.into_iter().collect());
                Reply::ok(json!({"closed":request.target}))
            }
            "term.pane.split" => {
                let dir = match request.dir.as_deref() {
                    Some("h" | "horizontal") => crate::panes::SplitDir::Horizontal,
                    Some("v" | "vertical") => crate::panes::SplitDir::Vertical,
                    _ => return Reply::error("INVALID_ARGUMENT"),
                };
                if !tabs.select(tab) || !tabs.focus(id) {
                    return Reply::error("FORBIDDEN");
                }
                match tabs.split_active(dir) {
                    Ok(id) => Reply::ok(json!({"pane_id":id})),
                    Err(_) => Reply::error("RESOURCE_LIMIT"),
                }
            }
            _ => Reply::error("UNSUPPORTED"),
        }
    }
}

fn reference(s: &SessionIdentity) -> RecordRef {
    RecordRef {
        record_id: s.record_id,
        incarnation: s.incarnation,
        binding_generation: s.binding_generation,
    }
}
fn actor_key(actor: &BrokerPrincipal) -> String {
    match &actor.session {
        Some(s) => format!("{}:{:?}:{:?}", actor.unix_uid, s.record_id, s.incarnation),
        None => format!(
            "{}:{:?}:{:?}",
            actor.unix_uid, actor.broker_epoch, actor.connection_id
        ),
    }
}
fn principal_allowed(parent: &SessionRecord, actor: &BrokerPrincipal) -> bool {
    actor.validate().is_ok()
        && actor.unix_uid == parent.owner_uid
        && actor.owner_node == parent.owner_node
        && actor.broker_epoch == parent.broker_epoch
        && match &actor.session {
            None => parent.policy == Policy::DefaultOpen,
            Some(s) if s.role == Role::Term => {
                s.record_id == parent.record_id && s.incarnation == parent.incarnation
            }
            Some(s) => {
                s.role == Role::PaneShell
                    && s.parent_instance == Some(parent.instance_id)
                    && s.parent_incarnation == Some(parent.incarnation)
            }
        }
}
/// Shared pure capability/scope decision. All callers must additionally supply
/// current checked leases and hold the pane owner's commitment lock.
pub fn allows(
    parent: &SessionRecord,
    actor: &BrokerPrincipal,
    target: &Target,
    capability: Capability,
) -> bool {
    if !principal_allowed(parent, actor)
        || parent.instance_id != target.instance_id
        || parent.incarnation != target.incarnation
        || target.pane_generation.0 == 0
    {
        return false;
    }
    match &actor.session {
        None => parent.policy == Policy::DefaultOpen,
        Some(s) if s.role == Role::Term => {
            s.record_id == parent.record_id
                && s.incarnation == parent.incarnation
                && s.binding_generation == parent.binding_generation
                && s.capabilities.contains(&capability)
        }
        Some(s) => {
            s.role == Role::PaneShell
                && s.parent_instance == Some(parent.instance_id)
                && s.parent_incarnation == Some(parent.incarnation)
                && s.pane_id == Some(target.pane_id)
                && s.pane_generation == Some(target.pane_generation)
                && s.capabilities.contains(&capability)
        }
    }
}
