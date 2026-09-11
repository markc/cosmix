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
            let _ = sender.try_send(notice);
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
#[derive(Default)]
struct History {
    high_water: u64,
    entries: VecDeque<Entry>,
}
#[derive(Default)]
struct State {
    history: HashMap<String, History>,
    permits: Vec<(Option<RecordRef>, std::sync::Weak<Permit>)>,
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
        }
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
            {
                let mut state = self.state.lock().unwrap();
                if let Some(entry) = state.history.get_mut(&notice.actor).and_then(|h| {
                    h.entries
                        .iter_mut()
                        .find(|e| Some(DecimalU64(e.sequence)) == notice.request_id)
                }) {
                    entry.outcome = Some(Reply::ok(json!({
                        "operation_id":notice.request_id,
                        "status":if notice.written >= notice.expected as u64 { "completed" } else { "unknown" },
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
                .with_body(&json!({"target":notice.target,"request_id":notice.request_id,"status":"revoked","outcome":"partial_or_unknown","delivered_bytes_lower_bound":notice.written}).to_string());
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
        if capability == Capability::Execute {
            return Reply::error("UNSUPPORTED");
        }
        // RPCs run outside both the model and policy locks. A failed check
        // grants no authority, including to an otherwise ambient owner when
        // the recipient's own native attachment has become unavailable.
        let checked = tokio::time::timeout(Duration::from_secs(2), async {
            let hello = connection.session_hello().await.ok()?;
            let parent_deadline = connection
                .session_lease_check(parent.reference())
                .await
                .ok()?;
            let actor_deadline = if let Some(s) = &actor.session {
                Some(connection.session_lease_check(reference(s)).await.ok()?)
            } else {
                None
            };
            Some((hello, parent_deadline, actor_deadline))
        })
        .await;
        let Ok(Some((hello, parent_deadline, actor_deadline))) = checked else {
            return Reply::error("FORBIDDEN");
        };
        if !parent_deadline.is_live(&hello).unwrap_or(false)
            || actor_deadline
                .as_ref()
                .is_some_and(|a| !a.is_live(&hello).unwrap_or(false))
        {
            return Reply::error("FORBIDDEN");
        }
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
        });
        let mut tabs = self.terminal.lock().unwrap();
        if !permit.valid() || tabs.pane_by_id(target.pane_id.0).is_none() {
            return Reply::error("FORBIDDEN");
        }
        if property {
            let peer = cosmix_props::PeerIdentity {
                unix_uid: Some(actor.unix_uid),
                unix_gid: Some(actor.unix_gid),
                native_session: Some(actor.clone()),
                ..Default::default()
            };
            let parent = parent.clone();
            let target = target.clone();
            let permit = permit.clone();
            let token = cosmix_props::Capability::new(format!(
                "props.{}:term.pane:{}:{}:{}",
                if matches!(capability, Capability::ReadState | Capability::ReadContents) {
                    "read"
                } else {
                    "write"
                },
                target.pane_id.0,
                target.pane_generation.0,
                capability.as_str()
            ))
            .unwrap();
            let issued = token.clone();
            let policy = cosmix_props::AuthPolicy::new(move |peer| {
                if permit.valid()
                    && peer
                        .native_session
                        .as_ref()
                        .is_some_and(|actor| allows(&parent, actor, &target, capability))
                {
                    [issued.clone()].into_iter().collect()
                } else {
                    cosmix_props::CapabilitySet::empty()
                }
            });
            if !policy.resolve(&peer).contains(&token) {
                return Reply::error("FORBIDDEN");
            }
        }
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
            let total = state
                .history
                .values()
                .map(|h| h.entries.len())
                .sum::<usize>();
            if !state.history.contains_key(&identity) && state.history.len() >= ACTORS {
                return Reply::error("RESOURCE_LIMIT");
            }
            let history = state.history.entry(identity.clone()).or_default();
            while history
                .entries
                .front()
                .is_some_and(|e| e.at.elapsed() >= RETENTION)
            {
                history.entries.pop_front();
            }
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
            return Reply::error("RESOURCE_LIMIT");
        }
        if mutation {
            if reply.rc == 0 {
                let mut result: Value =
                    serde_json::from_str(&reply.body).expect("owned reply JSON");
                result["operation_id"] = json!(request.request_id);
                reply.body = result.to_string();
            }
            let history = state.history.get_mut(&identity).unwrap();
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
                tabs.select(tab);
                tabs.focus(id);
                Reply::ok(json!({"selected":request.target}))
            }
            "term.tab.close" => {
                let (_, removed) = tabs.close(tab);
                self.cleanup.submit(removed.into_iter().collect());
                Reply::ok(json!({"closed":request.target}))
            }
            "term.pane.close" => {
                tabs.select(tab);
                tabs.focus(id);
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
                tabs.select(tab);
                tabs.focus(id);
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
