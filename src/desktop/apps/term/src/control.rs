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
    /// Execute family. `source` is the line to run; `prompt_generation` is the
    /// generation the caller believes is at the child's prompt, which is what
    /// turns a stale snapshot into a refusal instead of an execution.
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    prompt_generation: Option<DecimalU64>,
    /// Task family. Relayed as sent — Term never resolves the source/argv
    /// union, so both-or-neither reaches the child's own refusal.
    #[serde(default)]
    argv: Option<Vec<String>>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    env: Vec<(String, String)>,
    #[serde(default)]
    timeout_ms: Option<DecimalU64>,
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
    /// Omitted entirely unless a refusal carries something the caller can act
    /// on. Denials stay uniform and detail-free so they reveal nothing.
    #[serde(skip_serializing_if = "Option::is_none")]
    details: Option<Value>,
}
impl Reply {
    fn ok(value: Value) -> Self {
        Self {
            rc: 0,
            body: value.to_string(),
        }
    }
    pub fn error(code: &'static str) -> Self {
        Self::refuse(code, None)
    }
    /// Only for refusals whose details tell an authorised caller how to make
    /// progress. Never attach details to a denial.
    fn refuse(code: &'static str, details: Option<Value>) -> Self {
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
            // A token this table does not know is a typo on the author's side,
            // not a caller's doing, and a request path must not panic over
            // one. Fail closed with the uniform denial and make it loud in a
            // debug build so a test catches the typo rather than production.
            _ => {
                debug_assert!(false, "unregistered Term error token: {code}");
                FailureCode::Forbidden
            }
        };
        Self {
            rc: 10,
            body: serde_json::to_string(&Failure {
                error_code,
                details,
            })
            .expect("owned failure schema"),
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
    /// Request ids Term mints for what it forwards, and the caller request each
    /// one stands for.
    ///
    /// Term forwards on its OWN connection, so at the child every agent's
    /// request ids land in one (actor, id) space keyed to Term. Relaying the
    /// caller's id therefore collapsed them together: agent B's id 1 replayed
    /// agent A's operation, and two agents that both used id 1 with different
    /// bodies conflicted with each other forever. Term mints its own
    /// monotonic sequence instead, so the child sees one id per
    /// (Term, forwarded-seq) and nothing collides.
    forwarded: HashMap<String, (u64, [u8; 32])>,
    next_forward: u64,
    /// Operation ids the child minted, and the ACTOR each belongs to.
    ///
    /// Every forwarded submission travels on Term's one connection, so at the
    /// child they all share Term's identity and the child's own per-actor
    /// scoping cannot separate them. Ownership therefore has to be kept HERE:
    /// without it a sibling agent that guesses an operation id reads another
    /// agent's result — up to 16 KiB of whatever that shell printed — or
    /// cancels its evaluation.
    operations: HashMap<u64, (String, Family)>,
}

/// Which surface an operation belongs to.
///
/// Scopes ADDRESSING only. The forwarded-id map is deliberately NOT split by
/// this: one caller request id must map to one forwarded id across both
/// surfaces, or the same id could mint a fresh operation on each and both would
/// execute.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Family {
    Execute,
    Task,
}
impl Family {
    fn of(verb: &str) -> Self {
        if verb.starts_with("term.task.") {
            Self::Task
        } else {
            Self::Execute
        }
    }
}

/// What Term already knows about a caller request it is being asked to forward.
enum Forward {
    Fresh(u64),
    /// Already forwarded under this id. Re-forwarding reaches the child's own
    /// dedupe, which is the only place that can say what actually happened.
    Retry(u64),
    Conflict,
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
    /// Drained by exactly one caller (the identity actor, through
    /// flush_notices). The mutex makes a second drainer safe rather than
    /// expected: two would interleave notices and each would see only part of
    /// the queue, so a revocation could be recorded without being sent.
    notice_rx: Mutex<tokio::sync::mpsc::Receiver<InputNotice>>,
    wake: Arc<tokio::sync::Notify>,
    /// Debug-only single-flight detector for the invariant on `dispatch`.
    #[cfg(debug_assertions)]
    serving: std::sync::atomic::AtomicBool,
}
/// Clears the single-flight flag however dispatch returns, including its many
/// early refusals.
#[cfg(debug_assertions)]
struct SingleFlight<'a>(&'a std::sync::atomic::AtomicBool);
#[cfg(debug_assertions)]
impl Drop for SingleFlight<'_> {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::Release);
    }
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
            #[cfg(debug_assertions)]
            serving: std::sync::atomic::AtomicBool::new(false),
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
        // A total budget, not just a per-send one: this still runs on the
        // identity loop, which must get back to renewing the attachment.
        let budget = Instant::now() + Duration::from_millis(200);
        for _ in 0..16 {
            if Instant::now() >= budget {
                break;
            }
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
                .with_body(&json!({"target":&notice.target,"request_id":notice.request_id,"status":"revoked","outcome":outcome,"delivered_bytes_lower_bound":notice.written}).to_string());
            if tokio::time::timeout(
                Duration::from_millis(100),
                connection.client().send_raw(&message),
            )
            .await
            .is_err()
            {
                // Put it back rather than lose it on the floor. It was already
                // dequeued, and the retained outcome recorded above is
                // idempotent, so a later wake can try the send again.
                let _ = self.notice_tx.try_send(notice);
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

    /// INVARIANT: one caller at a time. The dedupe check and the commit that
    /// follows it do NOT hold a single lock across the whole decision, so two
    /// concurrent callers could both pass the check for the same request ID and
    /// both execute. The identity actor's server task is that single caller by
    /// construction — it serves jobs serially off one queue — and the debug
    /// guard below fails loudly if a second entry point is ever added.
    pub async fn dispatch(
        &self,
        connection: &VerifiedConnection,
        parent: &SessionRecord,
        own: &(Hello, Deadline),
        event: &VerifiedCommand,
    ) -> Reply {
        #[cfg(debug_assertions)]
        let _single_flight = {
            assert!(
                !self
                    .serving
                    .swap(true, std::sync::atomic::Ordering::AcqRel),
                "dispatch is not re-entrant: the dedupe check and its commit are not one atomic step"
            );
            SingleFlight(&self.serving)
        };
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
        // An unknown verb is resolved against the WEAKEST capability rather
        // than answered here. Answering early would tell an unauthorised caller
        // which verbs exist — a known one comes back FORBIDDEN and an unknown
        // one UNSUPPORTED, which is a probe of the verb table. The UNSUPPORTED
        // fall-through is below, after `allows`.
        let capability =
            capability_of(verb, request.contents).unwrap_or(Capability::ReadState);
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
        // Only now, to a caller that WOULD have been allowed. An unauthorised
        // one was refused above and learns nothing about the verb table.
        //
        // Derived from the capability match, not a third hand-written copy of
        // it: the same list already existed there and in the coverage test, and
        // a fourth would drift the way the second one did.
        if capability_of(verb, request.contents).is_none() {
            return Reply::error("UNSUPPORTED");
        }
        if matches!(verb, "term.tab.new" | "term.pane.split")
            && actor
                .session
                .as_ref()
                .is_some_and(|s| s.role == Role::PaneShell)
        {
            return Reply::error("FORBIDDEN");
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
                        Reply::refuse("CONFLICT", Some(mismatch()))
                    };
                }
                if sequence.0 <= history.high_water {
                    return Reply::refuse("UNKNOWN_OUTCOME", Some(retired(history.high_water)));
                }
            }
        }
        // The execute family answers from the CHILD, over a round trip Term
        // must not hold the terminal or tab lock across. It is handled here,
        // after every identity, capability and lease check and before any of
        // those locks are taken.
        if matches!(
            verb,
            "term.execute"
                | "term.exec.result"
                | "term.exec.cancel"
                | "term.task.submit"
                | "term.task.result"
                | "term.task.cancel"
        ) {
            return self
                .forward_execute(connection, actor, verb, &request, &identity, digest)
                .await;
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
        if mutation {
            let Some(sequence) = request.request_id.map(|id| id.0).filter(|id| *id > 0) else {
                return Reply::error("INVALID_ARGUMENT");
            };
            if let Err(reply) = self.reserve_request_id(&identity, sequence, digest) {
                return reply;
            }
        }
        let mut state = self.state.lock().unwrap();
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

    /// BROKER-022's commit-before-execute step, and the only copy of it. The
    /// id is retired BEFORE the verb runs so a mutation whose result was lost
    /// can never re-execute; `Err` carries the answer the caller gets instead.
    fn reserve_request_id(
        &self,
        identity: &str,
        sequence: u64,
        digest: [u8; 32],
    ) -> Result<(), Reply> {
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
        if !state.history.contains_key(identity) && state.history.len() >= ACTORS {
            let victim = state
                .history
                .iter()
                .min_by_key(|(_, history)| (!history.entries.is_empty(), history.last))
                .map(|(key, _)| key.clone());
            if let Some(key) = victim {
                state.history.remove(&key);
            }
        }
        let history = state.history.entry(identity.to_owned()).or_default();
        if let Some(entry) = history.entries.iter().find(|e| e.sequence == sequence) {
            return Err(if entry.digest == digest {
                entry.reply.clone()
            } else {
                Reply::refuse("CONFLICT", Some(mismatch()))
            });
        }
        if sequence <= history.high_water {
            return Err(Reply::refuse("UNKNOWN_OUTCOME", Some(retired(history.high_water))));
        }
        if total >= TOTAL {
            return Err(Reply::error("RESOURCE_LIMIT"));
        }
        history.high_water = sequence;
        Ok(())
    }

    /// The id Term forwards for one caller request, minted once and remembered
    /// so a byte-identical retry forwards the SAME id and reaches the child's
    /// own dedupe rather than becoming a second submission.
    fn forwarded_id(&self, identity: &str, sequence: u64, digest: [u8; 32]) -> Forward {
        let mut state = self.state.lock().unwrap();
        let key = format!("{identity}\0{sequence}");
        if let Some((existing, recorded)) = state.forwarded.get(&key) {
            return if *recorded == digest {
                Forward::Retry(*existing)
            } else {
                Forward::Conflict
            };
        }
        state.next_forward += 1;
        let minted = state.next_forward;
        // Bounded alongside the history it belongs to. Evict ONE key, not
        // the whole table: clearing it would strip every live caller of its
        // retry path at once, turning a capacity event into a fleet-wide
        // outage of exactly the property the mapping exists to provide.
        if state.forwarded.len() >= TOTAL
            && let Some(victim) = state.forwarded.keys().next().cloned()
        {
            state.forwarded.remove(&victim);
        }
        state.forwarded.insert(key, (minted, digest));
        Forward::Fresh(minted)
    }

    /// Bind a child-minted operation id to the actor that caused it.
    fn claim_operation(&self, identity: &str, operation: u64, family: Family) {
        let mut state = self.state.lock().unwrap();
        // Bounded with the history it belongs to; an id whose mapping is gone
        // answers like an unknown one, which is a refusal and never a leak.
        if state.operations.len() >= TOTAL
            && let Some(victim) = state.operations.keys().next().copied()
        {
            state.operations.remove(&victim);
        }
        state.operations.insert(operation, (identity.to_owned(), family));
    }
    /// Whether `identity` may address `operation` at all. An unmapped id is
    /// treated as somebody else's, not as public.
    fn owns_operation(&self, identity: &str, operation: u64, family: Family) -> bool {
        self.state
            .lock()
            .unwrap()
            .operations
            .get(&operation)
            .is_some_and(|(owner, owned_family)| owner == identity && *owned_family == family)
    }

    /// Retain a completed mutation's outcome so a retry replays it.
    fn record_outcome(
        &self,
        identity: &str,
        target: &Target,
        sequence: u64,
        digest: [u8; 32],
        reply: &Reply,
    ) {
        let mut state = self.state.lock().unwrap();
        let Some(history) = state.history.get_mut(identity) else {
            return;
        };
        history.last = Instant::now();
        history.entries.push_back(Entry {
            target: target.clone(),
            outcome: None,
            sequence,
            digest,
            at: Instant::now(),
            reply: reply.clone(),
        });
        if history.entries.len() > PER_ACTOR {
            history.entries.pop_front();
        }
    }

    /// BROKER-023 `execute`, forwarded to the pane shell's own surface.
    ///
    /// Term does not decide whether an execution may happen — the child does,
    /// against ITS prompt, ITS editor state and ITS generation, none of which
    /// Term can observe. What Term owns is the same thing it owns for
    /// `term.type`: the actor/target/retry rules, and the guarantee that the
    /// request reaches the child this pane is actually bound to at exactly this
    /// generation. It holds no terminal or tab lock across the forward, because
    /// the child's answer may take a full admission round trip.
    async fn forward_execute(
        &self,
        connection: &VerifiedConnection,
        actor: &BrokerPrincipal,
        verb: &str,
        request: &Request,
        identity: &str,
        digest: [u8; 32],
    ) -> Reply {
        let Some(child) = self.native.child_binding(
            request.target.pane_id.0,
            request.target.pane_generation.0,
        ) else {
            return Reply::error("FORBIDDEN");
        };
        // The child's own Source, not this pane's Target. They name the same
        // pane through different identities, and the child will refuse anything
        // that is not exactly its own.
        let child_target = json!({
            "broker_epoch": child.broker_epoch,
            "record": child.reference(),
            "instance_id": child.instance_id,
            "pane_id": child.pane_id,
            "pane_generation": child.pane_generation,
        });
        let (shell_verb, body, sequence) = match verb {
            "term.execute" => {
                let (Some(source), Some(generation)) =
                    (request.source.as_deref(), request.prompt_generation)
                else {
                    return Reply::error("INVALID_ARGUMENT");
                };
                let Some(sequence) = request.request_id.map(|id| id.0).filter(|id| *id > 0) else {
                    return Reply::error("INVALID_ARGUMENT");
                };
                // A submission is a mutation: it must carry the connection that
                // is making it, so a reconnected caller cannot inherit an
                // in-flight id.
                match request.request_epoch {
                    None => return Reply::error("INVALID_ARGUMENT"),
                    Some(epoch) if epoch != actor.connection_id => {
                        return Reply::error("UNKNOWN_OUTCOME");
                    }
                    Some(_) => {}
                }
                // Order matters. A caller request Term has ALREADY forwarded
                // maps to a stable child id, so re-forwarding it cannot execute
                // twice — the child's dedupe owns that decision. Consulting the
                // mapping first is what lets a retry get past Term's own
                // high-water mark, which is otherwise the thing that turns
                // "your answer was lost" into "unknown, forever".
                let forwarded = match self.forwarded_id(identity, sequence, digest) {
                    Forward::Conflict => return Reply::refuse("CONFLICT", Some(mismatch())),
                    Forward::Retry(id) => id,
                    Forward::Fresh(id) => {
                        if let Err(reply) = self.reserve_request_id(identity, sequence, digest) {
                            return reply;
                        }
                        id
                    }
                };
                (
                    "shell.execute",
                    json!({
                        "version": 1,
                        "target": child_target,
                        "request_id": DecimalU64(forwarded),
                        "prompt_generation": generation,
                        "source": source,
                        // The shell authenticated Term, not this actor, so the
                        // shell renders it as relayed. Taken from Term's own
                        // trusted context — a caller cannot put a name here.
                        "on_behalf_of": principal_label(actor),
                    }),
                    Some(sequence),
                )
            }
            // A task submission mirrors the evaluation one mechanically: same
            // epoch rule, same forwarded-id mapping, same retry discipline.
            // What it does NOT carry is `on_behalf_of` — a task never renders
            // into the pane, so there is no announcement for a principal to
            // appear in, and sending the field would be refused by the child's
            // `deny_unknown_fields`.
            "term.task.submit" => {
                let Some(sequence) = request.request_id.map(|id| id.0).filter(|id| *id > 0) else {
                    return Reply::error("INVALID_ARGUMENT");
                };
                match request.request_epoch {
                    None => return Reply::error("INVALID_ARGUMENT"),
                    Some(epoch) if epoch != actor.connection_id => {
                        return Reply::error("UNKNOWN_OUTCOME");
                    }
                    Some(_) => {}
                }
                // The SAME forwarded map as evaluations, keyed (identity,
                // sequence) and not split by kind: splitting it would let one
                // caller request id mint a fresh forwarded id on each surface
                // and BOTH execute.
                let forwarded = match self.forwarded_id(identity, sequence, digest) {
                    Forward::Conflict => return Reply::refuse("CONFLICT", Some(mismatch())),
                    Forward::Retry(id) => id,
                    Forward::Fresh(id) => {
                        if let Err(reply) = self.reserve_request_id(identity, sequence, digest) {
                            return reply;
                        }
                        id
                    }
                };
                let mut body = json!({
                    "version": 1,
                    "target": child_target,
                    "request_id": DecimalU64(forwarded),
                    "env": request.env,
                });
                // Absent fields are OMITTED, never sent as null. A relayed null
                // is a present field of the wrong type, so the child answers
                // INVALID_REQUEST — a malformed-body complaint — where it
                // should be applying its own default or answering
                // INVALID_ARGUMENT about a value the caller actually chose.
                if let Some(cwd) = &request.cwd {
                    body["cwd"] = json!(cwd);
                }
                if let Some(timeout_ms) = request.timeout_ms {
                    body["timeout_ms"] = json!(timeout_ms);
                }
                // The discriminated union is relayed as the caller sent it;
                // Term does not choose a side, so "both" and "neither" reach
                // the child's own refusal rather than being resolved here.
                if let Some(source) = &request.source {
                    body["source"] = json!(source);
                }
                if let Some(argv) = &request.argv {
                    body["argv"] = json!(argv);
                }
                ("shell.task.submit", body, Some(sequence))
            }
            // Reading a result and cancelling are idempotent against one
            // immutable identity, so neither spends a request id.
            _ => {
                let Some(operation) = request.operation_id else {
                    return Reply::error("INVALID_ARGUMENT");
                };
                let family = Family::of(verb);
                // Scoped to the actor that submitted it AND to the family it
                // belongs to, refused exactly like an id that does not exist —
                // so neither surface can read the other's operations, and
                // neither can be used to discover which numbers are live.
                if !self.owns_operation(identity, operation.0, family) {
                    return Reply::error("UNKNOWN_OUTCOME");
                }
                let shell_verb = match verb {
                    "term.exec.cancel" => "shell.execute.cancel",
                    "term.task.cancel" => "shell.task.cancel",
                    "term.task.result" => "shell.task.result",
                    _ => "shell.execute.result",
                };
                (
                    shell_verb,
                    json!({
                        "version": 1,
                        "target": child_target,
                        "operation_id": operation,
                    }),
                    None,
                )
            }
        };
        let answer = tokio::time::timeout(
            Duration::from_secs(5),
            connection.client().call(&child.name, shell_verb, body),
        )
        .await;
        // `retain` is the load-bearing distinction. A reply that came FROM the
        // child is that request's settled outcome and is retained so a retry
        // replays it. A local placeholder — Term's own timeout, or a reply too
        // large to deliver — is not an outcome at all: retaining it would make
        // every byte-identical retry replay the placeholder forever, when
        // re-forwarding would reach the child's own dedupe and get the real
        // answer. So placeholders are returned and NOT recorded.
        let (mut reply, mut retain) = match answer {
            Ok(Ok(value)) => {
                let mut value = value;
                // Record who owns the operation the child just minted, before
                // the id reaches anyone. A sibling that learns the number some
                // other way still cannot address it.
                if let Some(operation) = value["operation_id"]
                    .as_str()
                    .and_then(|id| id.parse::<u64>().ok())
                {
                    self.claim_operation(identity, operation, Family::of(verb));
                }
                value["target"] = json!(request.target);
                if let Some(sequence) = sequence {
                    value["operation_id_request"] = json!(DecimalU64(sequence));
                }
                (Reply::ok(value), true)
            }
            // The child's refusal is ITS answer about ITS prompt. Term relays
            // it rather than replacing it with a guess, because BUSY and
            // STALE_GENERATION tell the caller two different things to do next.
            Ok(Err(error)) => {
                let body = error.to_string();
                // A refusal about the child's own LOAD settles nothing about
                // this request. Retaining one replays "too many tasks" for that
                // id forever, while the documented move for both codes is to
                // back off and retry the same submission — which only reaches
                // the child's dedupe if Term did not record the refusal.
                let transient = matches!(refusal_code(&body).as_deref(), Some("RESOURCE_LIMIT"));
                (shell_refusal(&body, sequence.is_some()), !transient)
            }
            // A submission whose answer never arrived may or may not have been
            // admitted. The caller's route forward is `term.exec.result`, or a
            // byte-identical retry that re-forwards to the child's dedupe —
            // which is only possible because this is not recorded.
            //
            // The REASON is logged because it cannot be relayed: the caller is
            // told "unknown", which is all Term honestly knows about its
            // request, but an operator holding the logs should not have to
            // guess whether the child was slow, gone, or never asked.
            Err(error) if sequence.is_some() => {
                eprintln!("term control: {verb} -> {shell_verb} child call failed ({error}); reporting unknown outcome");
                (Reply::error("UNKNOWN_OUTCOME"), false)
            }
            Err(error) => {
                eprintln!("term control: {verb} -> {shell_verb} child call failed ({error})");
                (Reply::error("DISCONNECTED"), false)
            }
        };
        if reply.body.len() > 256 * 1024 {
            reply = Reply::error(if sequence.is_some() {
                "UNKNOWN_OUTCOME"
            } else {
                "RESOURCE_LIMIT"
            });
            retain = false;
        }
        if let Some(sequence) = sequence
            && retain
        {
            self.record_outcome(identity, &request.target, sequence, digest, &reply);
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

/// The child's own error code, if its answer was shaped like a refusal at all.
fn refusal_code(body: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()?
        .get("error_code")?
        .as_str()
        .map(str::to_owned)
}

/// Translate the pane shell's refusal into Term's vocabulary. The child speaks
/// almost the same one; the two that differ are spelled differently for the
/// same meaning, and anything unrecognised is reported as an unknown outcome
/// rather than being flattened into a denial that would read as a policy
/// decision Term never made.
fn shell_refusal(body: &str, mutation: bool) -> Reply {
    #[derive(Deserialize)]
    struct Refusal {
        error_code: String,
        #[serde(flatten)]
        rest: serde_json::Map<String, Value>,
    }
    let refusal = serde_json::from_str::<Refusal>(body).ok();
    let code = refusal
        .as_ref()
        .map(|r| r.error_code.clone())
        .unwrap_or_default();
    // The child's refusals CARRY things a caller has to act on: the
    // `operation_id` to ask `term.exec.result` about, and the `reason` that
    // separates "provably did not start, use a new id" from "may have run, go
    // and find out". Relaying only the code discarded both, which left the
    // three-refusal contract unreachable on the only real agent path — the one
    // where every submission goes through Term.
    let relayed = |mapped: &'static str| {
        let details = refusal.as_ref().map(|r| &r.rest).filter(|rest| !rest.is_empty());
        match details {
            Some(rest) => Reply::refuse(mapped, Some(Value::Object(rest.clone()))),
            None => Reply::error(mapped),
        }
    };
    match code.as_str() {
        "BUSY" => relayed("BUSY"),
        "STALE_GENERATION" => relayed("STALE_GENERATION"),
        "UNSUPPORTED" => Reply::error("UNSUPPORTED"),
        "RESOURCE_LIMIT" => Reply::error("RESOURCE_LIMIT"),
        // The child named something the caller gave it that is not there: a
        // `cwd` that does not exist, a program that cannot be found. Relayed
        // with its details, because `reason` and `detail` are what tell the
        // caller WHICH thing was missing. Its absence here is why a perfectly
        // clear child refusal was reaching callers as "unknown outcome".
        "NOT_FOUND" => relayed("NOT_FOUND"),
        "CONFLICT" => Reply::refuse("CONFLICT", Some(mismatch())),
        "UNKNOWN_OUTCOME" => relayed("UNKNOWN_OUTCOME"),
        "INVALID_REQUEST" => Reply::error("INVALID_ARGUMENT"),
        // A settled denial: the child looked at the request and said no.
        "REFUSED" => Reply::error("FORBIDDEN"),
        // Everything else is a shape Term does not recognise. Flattening those
        // to FORBIDDEN was wrong in both directions: it reads as a policy
        // decision Term never made, and for a transient it tells the caller to
        // stop when it should retry. An unrecognised answer to a mutation is an
        // unknown outcome; to a read, a transport-shaped failure.
        //
        // Logged with the BODY, because this arm is where a new child code, or
        // an answer that was never a refusal at all, disappears without trace.
        // The caller is told "unknown" — which is true — but that is no reason
        // for the operator to be told nothing.
        _ if mutation => {
            eprintln!("term control: unrecognised child answer, reporting unknown outcome: {body}");
            Reply::error("UNKNOWN_OUTCOME")
        }
        _ => {
            eprintln!("term control: unrecognised child answer: {body}");
            Reply::error("DISCONNECTED")
        }
    }
}

/// The dedupe digest covers the request bytes as sent, deliberately: this
/// recipient does not canonicalise, so two encodings of the same object are two
/// different payloads. A retry MUST replay the body byte for byte, and the
/// refusal says so rather than leaving a caller to guess why its "identical"
/// retry conflicted.
fn mismatch() -> Value {
    json!({"reason":"request_mismatch","retry_requires":"byte_identical_body"})
}
/// The high-water mark is advanced before the verb runs, so an ID can be retired
/// without its outcome ever being recorded (BROKER-022: a committed mutation
/// must never re-execute, even when its result was lost). An actor holding only
/// Input or Terminate cannot read the mark back through term.session, so the
/// refusal carries the floor it has to climb past to make progress again.
fn retired(high_water: u64) -> Value {
    json!({"reason":"retired_request_id","request_high_water":DecimalU64(high_water)})
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
/// The ONE verb-to-capability table. `dispatch`'s gate, its UNSUPPORTED
/// fall-through and the grant-coverage test all read this, because the moment
/// there were two copies the second one drifted — it kept answering UNSUPPORTED
/// before `allows`, which is how an unauthorised caller could tell a real verb
/// from an invented one.
pub(crate) fn capability_of(verb: &str, contents: bool) -> Option<Capability> {
    Some(match verb {
        "term.session" | "term.list" | "term.tabs" | "term.panes" | "term.operation" => {
            Capability::ReadState
        }
        "term.snapshot" if !contents => Capability::ReadState,
        "term.snapshot" => Capability::ReadContents,
        "term.type" => Capability::Input,
        "term.tab.new" | "term.tab.select" | "term.pane.split" | "term.pane.select" => {
            Capability::ManageLayout
        }
        "term.tab.close" | "term.pane.close" => Capability::Terminate,
        // One capability for the whole execute family. Asking what an execution
        // did is asking about an execution.
        "term.execute" | "term.exec.result" | "term.exec.cancel" => Capability::Execute,
        // Same authority: an isolated task is still this principal causing
        // this shell to run code. The isolation is about the process.
        "term.task.submit" | "term.task.result" | "term.task.cancel" => Capability::Execute,
        _ => return None,
    })
}

/// The name the PANE announces for a forwarded submission. Built from Term's
/// own broker-stamped actor context and nothing else: a caller has no field
/// through which to supply a name, because a name a caller chose would let one
/// agent announce itself as another.
fn principal_label(actor: &BrokerPrincipal) -> String {
    match &actor.session {
        Some(s) => {
            let record: String = s
                .record_id
                .0
                .iter()
                .take(4)
                .map(|byte| format!("{byte:02x}"))
                .collect();
            format!("{:?} {}", s.role, record)
        }
        None => format!("uid {} pid {}", actor.unix_uid, actor.peer_pid),
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
