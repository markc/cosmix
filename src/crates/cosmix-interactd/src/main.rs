//! cosmix-interactd — the headless ctkd interaction daemon (notify.v1).
//!
//! Registers the Bus `interact` service and serves passive notifications: the
//! first, *renderless* ctkd surface. This bin owns the transport, the clock, and
//! handle minting; the decision policy is [`cosmix_interaction_broker`] and the
//! wire vocabulary is [`cosmix_interaction_schema`]. It draws nothing — no Bevy.
//!
//! Verbs:
//!
//! - `interact.notify`  — `NotifyRequest` → queue, return immediately
//!   `{ "handle": …, "owner_token": … }`
//!   (or `{ "throttled": true, … }` when rate-limited). Actions dispatch on
//!   click to a registered Bus service (validated asynchronously before show).
//! - `interact.update`  — `{ "handle", "owner_token", …NotifyRequest }` →
//!   queue an in-place re-render (summary/body/urgency only; actions are fixed).
//! - `interact.dismiss` — `{ "handle", "owner_token" }` → close + mark terminal
//!   (idempotent).
//! - `interact.props.*` — L2 event-driven view of the live `interactions` collection.
//!
//! A bounded worker owns the [`sink::NotifySink`]: [`sink::FreedesktopSink`]
//! draws real desktop toasts (default), while [`sink::RecordingSink`] logs intent
//! for headless hosts. An invoked action button fires `send <service> <verb>
//! handle=<h> key=<k>`, so callbacks survive the caller exiting. See the spec:
//! `~/.cmctl/_doc/2026-07-22-notify-v1-spec.md`.

mod props;
mod sink;
mod state;

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clap::{Parser, Subcommand, ValueEnum};
use cosmix_client::{IncomingCommand, NodedClient};
use cosmix_interaction_broker::{
    Caller, DialogBrokerError, RejectReason, validate_dispatch_targets,
};
use cosmix_interaction_schema::{
    DIALOG_RAW_INGRESS_CAP, DialogCancelRequest, DialogOpenRequestV1, DialogPresenterFailRequestV1,
    DialogPresenterMarkPresentedRequestV1, DialogPresenterNextRequestV1,
    DialogPresenterProgressCancelRequestV1, DialogPresenterRegisterRequestV1,
    DialogPresenterReleaseRequestV1, DialogPresenterResolveRequestV1,
    DialogProgressCompleteRequest, DialogProgressUpdateRequest, DialogResultRequest, IconRef,
    InteractionHandle, MAX_DELIVERY_QUEUE_BYTES, MAX_REQUEST_BYTES, NotifyCreateRequest,
    NotifyDismissRequest, NotifyHandle, NotifyRequest, NotifyUpdateRequest, OwnerToken,
    VERB_DIALOG_CANCEL, VERB_DIALOG_OPEN, VERB_DIALOG_PROGRESS_COMPLETE,
    VERB_DIALOG_PROGRESS_UPDATE, VERB_DIALOG_RESULT, VERB_PRESENTER_FAIL,
    VERB_PRESENTER_MARK_PRESENTED, VERB_PRESENTER_NEXT, VERB_PRESENTER_PROGRESS_CANCEL,
    VERB_PRESENTER_REGISTER, VERB_PRESENTER_RELEASE, VERB_PRESENTER_RESOLVE, is_bus_service_name,
};
use cosmix_mesh_trust::ctk_caps::CTK_NOTIFY;
use cosmix_props_core::PropTree;
use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;

use props::{InteractionsProps, PROPS_CHANGED_TOPIC, gap_message, transition_message};
use sink::{
    FreedesktopSink, NotifySink, RecordingSink, ShowOutcome, SinkEvent, SinkEventSenders,
    TombstoneResolution,
};
use state::{
    ClosedKind, DeliveryCompletion, DeliveryJob, DeliverySignal, DialogPropsTransition,
    DispatchOrder, InteractState, NotifyOwnership, OwnerId, PropsStateTransition, SignalResult,
    TerminalOutcome,
};

const DELIVERY_QUEUE_CAP: usize = 256;
const SIGNAL_QUEUE_CAP: usize = 128;
const ACTION_QUEUE_CAP: usize = 128;
const PROPS_EVENT_QUEUE_CAP: usize = 256;
const AMBIGUOUS_SHOW_CAP: usize = 256;
const IO_TIMEOUT: Duration = Duration::from_secs(5);

/// Header noded stamps on every routed delivery with the source socket's
/// locality, overwriting any client spelling (it strips then re-stamps from the
/// source IP, so a mesh peer cannot forge `local`). Wire-mirrored from
/// cosmix-noded `subscription::BROKER_ORIGIN_HEADER`; interactd cannot import
/// the `pub(crate)` constant, so it is duplicated here with this provenance.
const BROKER_ORIGIN_HEADER: &str = "broker_origin";
/// The `broker_origin` value noded stamps for node-local (loopback/same-node)
/// deliveries; the mesh value is `"mesh"`.
const BROKER_ORIGIN_LOCAL: &str = "local";

#[derive(Debug, Clone, PartialEq)]
enum PropsEvent {
    Notification(PropsStateTransition),
    Dialog(DialogPropsTransition),
    Resync { seq: u64, snapshot: Value },
}

impl PropsEvent {
    fn seq(&self) -> u64 {
        match self {
            Self::Notification(transition) => transition.seq,
            Self::Dialog(transition) => transition.seq,
            Self::Resync { seq, .. } => *seq,
        }
    }

    fn description(&self) -> String {
        match self {
            Self::Notification(transition) => format!(
                "notification {} state {:?}",
                transition.handle.as_str(),
                transition.new
            ),
            Self::Dialog(transition) => format!(
                "dialog {} state {:?}",
                transition.handle.as_str(),
                transition.new
            ),
            Self::Resync { seq, .. } => format!("full snapshot resync at seq {seq}"),
        }
    }
}

/// Bounded lifecycle-event ingress plus an out-of-band loss counter. A full
/// channel never blocks notify mutations; instead the next successfully
/// published event carries an explicit gap marker and loss count.
#[derive(Clone)]
struct PropsEventIngress {
    tx: mpsc::Sender<Vec<PropsEvent>>,
    loss: Arc<Mutex<PropsEventLoss>>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct PropsEventLoss {
    lost_count: u64,
    through_seq: u64,
}

impl PropsEventLoss {
    fn record(&mut self, transition: &PropsEvent) {
        self.lost_count = self.lost_count.saturating_add(1);
        self.through_seq = self.through_seq.max(transition.seq());
    }

    fn merge(&mut self, other: Self) {
        self.lost_count = self.lost_count.saturating_add(other.lost_count);
        self.through_seq = self.through_seq.max(other.through_seq);
    }
}

impl PropsEventIngress {
    fn channel(capacity: usize) -> (Self, mpsc::Receiver<Vec<PropsEvent>>) {
        let (tx, rx) = mpsc::channel(capacity);
        (
            Self {
                tx,
                loss: Arc::new(Mutex::new(PropsEventLoss::default())),
            },
            rx,
        )
    }

    fn enqueue(&self, transitions: Vec<PropsEvent>) {
        if transitions.is_empty() {
            return;
        }
        if let Err(error) = self.tx.try_send(transitions) {
            let transitions = error.into_inner();
            let mut loss = self.loss.lock().expect("props loss tracker poisoned");
            for transition in &transitions {
                loss.record(transition);
            }
            eprintln!(
                "cosmix-interactd: [props] event ingress full/closed; recorded {} lost transitions through seq {}",
                transitions.len(),
                loss.through_seq
            );
        }
    }
}

fn drain_props_events(state: &mut InteractState) -> Vec<PropsEvent> {
    let notifications = state.take_props_transitions();
    let dialogs = state.take_dialog_props_transitions();
    if state.take_dialog_props_resync_needed() {
        let props = InteractionsProps::from_records(
            state.records(),
            state.dialog_records(),
            state.props_event_seq(),
        );
        let snapshot: Value = (&props.snapshot()).into();
        return vec![PropsEvent::Resync {
            seq: state.props_event_seq(),
            snapshot,
        }];
    }

    notifications
        .into_iter()
        .map(PropsEvent::Notification)
        .chain(dialogs.into_iter().map(PropsEvent::Dialog))
        .collect()
}

/// One command's broker-authenticated local caller projection. `origin` is the
/// canonical noded-rewritten service label; mutation authority additionally
/// requires the per-notification owner token.
#[derive(Debug, Clone, PartialEq, Eq)]
struct AuthorizedCaller {
    owner: OwnerId,
    origin: String,
}

impl AuthorizedCaller {
    fn broker_caller(&self) -> Caller<'_> {
        Caller::local(&self.origin)
    }
}

#[derive(Debug)]
struct AuthorizationError {
    id: &'static str,
    message: String,
}

enum DeliveryCommand {
    Show {
        job: Box<DeliveryJob>,
        registry_client: Option<Arc<NodedClient>>,
        _bytes: DeliveryBytePermit,
    },
    Close {
        handle: NotifyHandle,
        _bytes: DeliveryBytePermit,
    },
}

#[derive(Clone)]
struct DeliveryByteBudget {
    used: Arc<AtomicUsize>,
    cap: usize,
}

impl DeliveryByteBudget {
    fn new(cap: usize) -> Self {
        Self {
            used: Arc::new(AtomicUsize::new(0)),
            cap,
        }
    }

    fn try_reserve(&self, bytes: usize) -> Option<DeliveryBytePermit> {
        let mut used = self.used.load(Ordering::Acquire);
        loop {
            let next = used.checked_add(bytes)?;
            if next > self.cap {
                return None;
            }
            match self
                .used
                .compare_exchange_weak(used, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => {
                    return Some(DeliveryBytePermit {
                        used: Arc::clone(&self.used),
                        bytes,
                    });
                }
                Err(current) => used = current,
            }
        }
    }
}

struct DeliveryBytePermit {
    used: Arc<AtomicUsize>,
    bytes: usize,
}

impl Drop for DeliveryBytePermit {
    fn drop(&mut self) {
        self.used.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

#[derive(Parser)]
#[command(
    name = "cosmix-interactd",
    version,
    about = "Headless ctkd interaction daemon (notify.v1)"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Register the `interact` Bus service and serve notify.v1 until killed.
    Serve {
        /// Bus service name to register as.
        #[arg(long, default_value = "interact")]
        bus_service: String,
        /// Where accepted notifications are delivered.
        #[arg(long, value_enum, default_value_t = SinkKind::Freedesktop)]
        sink: SinkKind,
    },
}

/// Delivery backend for accepted notifications.
#[derive(Copy, Clone, Debug, ValueEnum)]
enum SinkKind {
    /// Real desktop toasts via the freedesktop notification daemon (needs a
    /// session bus + a running daemon). The default.
    Freedesktop,
    /// Log delivery intent to stderr/journald — for headless hosts with no
    /// desktop, and the no-daemon fallback.
    Recording,
}

impl SinkKind {
    fn build(self, events: SinkEventSenders) -> Box<dyn NotifySink> {
        match self {
            SinkKind::Freedesktop => Box::new(FreedesktopSink::new(events)),
            SinkKind::Recording => Box::new(RecordingSink::default()),
        }
    }
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().cmd {
        Cmd::Serve { bus_service, sink } => {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            let state = Arc::new(Mutex::new(InteractState::new()));
            rt.block_on(serve(bus_service, sink, state))
        }
    }
}

/// The reconnect loop: (re)register with noded and serve until the connection
/// drops, then back off and retry. Never returns under normal operation.
async fn serve(
    service: String,
    sink: SinkKind,
    state: Arc<Mutex<InteractState>>,
) -> anyhow::Result<()> {
    let bi = cosmix_buildinfo::build_info!();
    let prov = cosmix_bus::RegisterProvenance::from_parts(
        bi.pkg,
        bi.version,
        bi.git_sha,
        bi.git_dirty,
        bi.build_time,
        cosmix_buildinfo::now_rfc3339(),
    );
    // A single long-lived dispatch task drains every notification's terminal
    // events (action clicks + closes). It outlives individual Bus connections —
    // a toast can be clicked long after the connection that created it dropped —
    // and reaches the current broker through the `watch` channel below.
    let (action_sig_tx, action_sig_rx) = mpsc::channel::<SinkEvent>(SIGNAL_QUEUE_CAP);
    let (terminal_sig_tx, terminal_sig_rx) = mpsc::channel::<SinkEvent>(SIGNAL_QUEUE_CAP);
    let (resolved_tx, resolved_rx) = mpsc::channel::<TerminalOutcome>(SIGNAL_QUEUE_CAP);
    let (delivery_tx, delivery_rx) = mpsc::channel::<DeliveryCommand>(DELIVERY_QUEUE_CAP);
    let delivery_bytes = DeliveryByteBudget::new(MAX_DELIVERY_QUEUE_BYTES);
    let (action_tx, action_rx) = mpsc::channel::<DispatchOrder>(ACTION_QUEUE_CAP);
    let (props_events, props_rx) = PropsEventIngress::channel(PROPS_EVENT_QUEUE_CAP);
    let (client_tx, client_rx) = watch::channel::<Option<Arc<NodedClient>>>(None);
    let initial_dialog_maintenance = state
        .lock()
        .expect("interact state poisoned")
        .dialog_next_maintenance_at_ms();
    let (dialog_maintenance_tx, dialog_maintenance_rx) = watch::channel(initial_dialog_maintenance);
    tokio::spawn(run_delivery(
        delivery_rx,
        sink.build(SinkEventSenders::new(action_sig_tx, terminal_sig_tx)),
        state.clone(),
        resolved_tx,
        props_events.clone(),
    ));
    tokio::spawn(run_dispatch(
        action_sig_rx,
        terminal_sig_rx,
        resolved_rx,
        state.clone(),
        action_tx,
        props_events.clone(),
    ));
    tokio::spawn(run_action_delivery(action_rx, client_rx.clone()));
    tokio::spawn(run_props_publisher(
        props_rx,
        client_rx,
        props_events.loss.clone(),
    ));
    tokio::spawn(run_dialog_maintenance(
        state.clone(),
        dialog_maintenance_rx,
        props_events.clone(),
    ));

    let mut backoff = Duration::from_secs(1);
    loop {
        match cosmix_config::client_helpers::connect_default_with_provenance(&service, prov.clone())
            .await
        {
            Ok(client) => {
                backoff = Duration::from_secs(1);
                let client = Arc::new(client);
                let _ = client_tx.send(Some(client.clone()));
                eprintln!(
                    "cosmix-interactd: registered as '{service}' (sink={sink:?}); serving notify.v1"
                );
                serve_bus(
                    client,
                    &state,
                    &delivery_tx,
                    &delivery_bytes,
                    &props_events,
                    &dialog_maintenance_tx,
                )
                .await;
                // Stop dispatching through the dead connection until we reconnect.
                let _ = client_tx.send(None);
                eprintln!("cosmix-interactd: broker disconnected; reconnecting");
            }
            Err(e) => {
                eprintln!("cosmix-interactd: broker unavailable; retry in {backoff:?}: {e}");
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }
}

/// Bounded FIFO for clicks observed while noded is disconnected. Overflow
/// drops the oldest intent so recent explicit user actions win.
struct PendingActions {
    queue: VecDeque<DispatchOrder>,
    cap: usize,
}

impl PendingActions {
    fn new(cap: usize) -> Self {
        Self {
            queue: VecDeque::with_capacity(cap),
            cap,
        }
    }

    fn push(&mut self, order: DispatchOrder) -> Option<DispatchOrder> {
        let dropped = (self.queue.len() == self.cap)
            .then(|| self.queue.pop_front())
            .flatten();
        self.queue.push_back(order);
        dropped
    }

    /// Remove an action before its single Bus send attempt. An error or timeout
    /// is ambiguous and therefore never puts the action back in the queue.
    fn take_for_attempt(&mut self) -> Option<DispatchOrder> {
        self.queue.pop_front()
    }
}

async fn send_action(client: &NodedClient, order: &DispatchOrder) -> bool {
    let services = match tokio::time::timeout(IO_TIMEOUT, client.list_services()).await {
        Ok(Ok(services)) => services,
        Ok(Err(error)) => {
            eprintln!(
                "cosmix-interactd: [notify] click dispatch {}.{} dropped: service registry revalidation failed: {error}",
                order.service, order.verb
            );
            return true;
        }
        Err(_) => {
            eprintln!(
                "cosmix-interactd: [notify] click dispatch {}.{} dropped: service registry revalidation timed out",
                order.service, order.verb
            );
            return true;
        }
    };
    if !action_target_is_registered(order, &services) {
        eprintln!(
            "cosmix-interactd: [notify] click dispatch {}.{} dropped: target is no longer registered",
            order.service, order.verb
        );
        return true;
    }

    let args = action_args(order);
    match tokio::time::timeout(IO_TIMEOUT, client.send(&order.service, &order.verb, args)).await {
        Ok(Ok(())) => {
            eprintln!(
                "cosmix-interactd: [notify] dispatched {}.{} handle={} key={}",
                order.service, order.verb, order.handle, order.key
            );
            true
        }
        Ok(Err(e)) => {
            eprintln!(
                "cosmix-interactd: [notify] dispatch {}.{} dropped after ambiguous send failure (at-most-once): {e}",
                order.service, order.verb
            );
            false
        }
        Err(_) => {
            eprintln!(
                "cosmix-interactd: [notify] dispatch {}.{} dropped after ambiguous send timeout (at-most-once)",
                order.service, order.verb
            );
            false
        }
    }
}

fn action_target_is_registered(order: &DispatchOrder, services: &[String]) -> bool {
    services.iter().any(|service| service == &order.service)
}

fn action_args(order: &DispatchOrder) -> Value {
    json!({
        "handle": order.handle,
        "key": order.key,
        "requested_by": order.requested_by,
    })
}

async fn flush_actions(client: &NodedClient, pending: &mut PendingActions) {
    flush_actions_with(
        pending,
        |order| async move { send_action(client, &order).await },
    )
    .await;
}

async fn flush_actions_with<F, Fut>(pending: &mut PendingActions, mut attempt: F)
where
    F: FnMut(DispatchOrder) -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    while let Some(order) = pending.take_for_attempt() {
        if !attempt(order).await {
            return;
        }
    }
}

fn handle_terminal_outcome(outcome: TerminalOutcome, action_tx: &mpsc::Sender<DispatchOrder>) {
    let TerminalOutcome::Action(outcome) = outcome else {
        return;
    };
    let Some(order) = outcome.dispatch else {
        return;
    };
    if let Err(error) = action_tx.try_send(order) {
        eprintln!(
            "cosmix-interactd: [notify] action delivery ingress full/closed; dropped resolved click: {error}"
        );
    }
}

/// Owns potentially slow Bus action sends. Reconnect flushes are event-driven
/// and may spend up to one deadline per action, but cannot monopolise desktop
/// lifecycle handling because this worker is independent of `run_dispatch`.
async fn run_action_delivery(
    mut action_rx: mpsc::Receiver<DispatchOrder>,
    mut client_rx: watch::Receiver<Option<Arc<NodedClient>>>,
) {
    let mut pending = PendingActions::new(ACTION_QUEUE_CAP);
    loop {
        tokio::select! {
            changed = client_rx.changed() => {
                if changed.is_err() {
                    return;
                }
                let client = client_rx.borrow().clone();
                if let Some(client) = client {
                    flush_actions(&client, &mut pending).await;
                }
            }
            order = action_rx.recv() => {
                let Some(order) = order else { return };
                let client = client_rx.borrow().clone();
                if pending.queue.is_empty()
                    && let Some(client) = client
                {
                    let _ = send_action(&client, &order).await;
                    continue;
                }
                if let Some(dropped) = pending.push(order) {
                    eprintln!(
                        "cosmix-interactd: [notify] reconnect action queue full; dropped oldest {}.{} handle={} key={}",
                        dropped.service, dropped.verb, dropped.handle, dropped.key
                    );
                }
            }
        }
    }
}

/// Bounded reconnect queue for the L2 property event surface. Delivery is
/// ordered and best-effort; every eviction increments the publisher loss count
/// so the next delivered event explicitly tells watchers to reseed.
struct PendingPropsEvents {
    queue: VecDeque<PropsEvent>,
}

impl PendingPropsEvents {
    fn new() -> Self {
        Self {
            queue: VecDeque::with_capacity(PROPS_EVENT_QUEUE_CAP),
        }
    }

    fn push(&mut self, transition: PropsEvent) -> Option<PropsEvent> {
        let dropped = (self.queue.len() == PROPS_EVENT_QUEUE_CAP)
            .then(|| self.queue.pop_front())
            .flatten();
        self.queue.push_back(transition);
        dropped
    }

    fn discard_all_into(&mut self, loss: &mut PropsEventLoss) {
        while let Some(discarded) = self.queue.pop_front() {
            loss.record(&discarded);
        }
    }
}

async fn publish_props_transition(client: &NodedClient, transition: &PropsEvent) -> bool {
    let message = transition_message(transition);
    publish_props_message(client, &message, &format!("seq {}", transition.seq())).await
}

async fn publish_props_gap(client: &NodedClient, loss: PropsEventLoss) -> bool {
    let message = gap_message(loss.through_seq, loss.lost_count);
    publish_props_message(
        client,
        &message,
        &format!(
            "gap through seq {} ({} lost)",
            loss.through_seq, loss.lost_count
        ),
    )
    .await
}

async fn publish_props_message(
    client: &NodedClient,
    message: &cosmix_bus::bus::BusMessage,
    description: &str,
) -> bool {
    let mut headers = BTreeMap::new();
    headers.insert("name".to_string(), PROPS_CHANGED_TOPIC.to_string());
    headers.insert("retain".to_string(), "false".to_string());
    match tokio::time::timeout(
        IO_TIMEOUT,
        client.send_with_headers("noded", "topic.publish", &headers, &message.to_wire()),
    )
    .await
    {
        Ok(Ok(())) => true,
        Ok(Err(error)) => {
            eprintln!(
                "cosmix-interactd: [props] props.changed publish failed for {description}: {error}"
            );
            false
        }
        Err(_) => {
            eprintln!(
                "cosmix-interactd: [props] props.changed publish timed out for {description}"
            );
            false
        }
    }
}

async fn flush_props_events(
    client: &NodedClient,
    pending: &mut PendingPropsEvents,
    props_rx: &mut mpsc::Receiver<Vec<PropsEvent>>,
    loss_tracker: &Mutex<PropsEventLoss>,
) {
    loop {
        if let Some(loss) = take_loss_and_discard_backlog(pending, props_rx, loss_tracker) {
            if !publish_props_gap(client, loss).await {
                loss_tracker
                    .lock()
                    .expect("props loss tracker poisoned")
                    .merge(loss);
                return;
            }
            continue;
        }

        let Some(transition) = pending.queue.front().cloned() else {
            return;
        };
        if !publish_props_transition(client, &transition).await {
            return;
        }
        pending.queue.pop_front();
    }
}

/// Atomically fast-forward the local event surface after any bounded loss.
/// Besides the publisher's `pending` queue, already-admitted batches can still
/// be waiting in `props_rx`; all of them precede (or extend) the gap watermark
/// and must be discarded before the gap frame is published. Otherwise an old
/// sequence could follow the gap and violate monotonic delivery.
fn take_loss_and_discard_backlog(
    pending: &mut PendingPropsEvents,
    props_rx: &mut mpsc::Receiver<Vec<PropsEvent>>,
    loss_tracker: &Mutex<PropsEventLoss>,
) -> Option<PropsEventLoss> {
    let mut loss = {
        let mut tracked = loss_tracker.lock().expect("props loss tracker poisoned");
        std::mem::take(&mut *tracked)
    };
    if loss.lost_count == 0 {
        return None;
    }

    loop {
        pending.discard_all_into(&mut loss);
        while let Ok(batch) = props_rx.try_recv() {
            for transition in &batch {
                loss.record(transition);
            }
        }

        let additional = {
            let mut tracked = loss_tracker.lock().expect("props loss tracker poisoned");
            std::mem::take(&mut *tracked)
        };
        if additional.lost_count == 0 {
            return Some(loss);
        }
        loss.merge(additional);
    }
}

/// Publishes lifecycle changes on the current broker connection. The worker is
/// independent of request, delivery, and click handling; a slow broker cannot
/// turn `interact.notify` back into a blocking operation.
async fn run_props_publisher(
    mut props_rx: mpsc::Receiver<Vec<PropsEvent>>,
    mut client_rx: watch::Receiver<Option<Arc<NodedClient>>>,
    loss_tracker: Arc<Mutex<PropsEventLoss>>,
) {
    let mut pending = PendingPropsEvents::new();
    loop {
        tokio::select! {
            changed = client_rx.changed() => {
                if changed.is_err() {
                    return;
                }
                let client = client_rx.borrow().clone();
                if let Some(client) = client {
                    flush_props_events(&client, &mut pending, &mut props_rx, &loss_tracker).await;
                }
            }
            batch = props_rx.recv() => {
                let Some(batch) = batch else { return };
                for transition in batch {
                    if let Some(dropped) = pending.push(transition) {
                        loss_tracker
                            .lock()
                            .expect("props loss tracker poisoned")
                            .record(&dropped);
                        eprintln!(
                            "cosmix-interactd: [props] reconnect queue full; recorded evicted {}",
                            dropped.description()
                        );
                    }
                }
                let client = client_rx.borrow().clone();
                if let Some(client) = client {
                    flush_props_events(&client, &mut pending, &mut props_rx, &loss_tracker).await;
                }
            }
        }
    }
}

/// Event-driven dialog maintenance. Mutations replace the watched absolute
/// deadline; the worker arms exactly one sleep and recomputes after it fires.
/// There is deliberately no periodic polling loop or sub-five-minute backstop.
async fn run_dialog_maintenance(
    state: Arc<Mutex<InteractState>>,
    mut schedule_rx: watch::Receiver<Option<u64>>,
    props_events: PropsEventIngress,
) {
    let mut scheduled = *schedule_rx.borrow_and_update();
    loop {
        let Some(at_ms) = scheduled else {
            if schedule_rx.changed().await.is_err() {
                return;
            }
            scheduled = *schedule_rx.borrow_and_update();
            continue;
        };

        let delay = Duration::from_millis(at_ms.saturating_sub(now_ms()));
        tokio::select! {
            changed = schedule_rx.changed() => {
                if changed.is_err() {
                    return;
                }
                scheduled = *schedule_rx.borrow_and_update();
            }
            () = tokio::time::sleep(delay) => {
                let (events, next) = {
                    let mut state = state.lock().expect("interact state poisoned");
                    state.dialog_maintain(now_ms());
                    let events = drain_props_events(&mut state);
                    let next = state.dialog_next_maintenance_at_ms();
                    (events, next)
                };
                props_events.enqueue(events);
                scheduled = next;
            }
        }
    }
}

fn reschedule_dialog_maintenance(state: &InteractState, schedule_tx: &watch::Sender<Option<u64>>) {
    schedule_tx.send_replace(state.dialog_next_maintenance_at_ms());
}

/// Drains same-connection desktop signals. Dedicated terminal ingress is
/// selected before best-effort clicks, and slow Bus delivery is handed to its
/// own bounded worker so state retirement cannot be held behind a reconnect.
async fn run_dispatch(
    mut action_sig_rx: mpsc::Receiver<SinkEvent>,
    mut terminal_sig_rx: mpsc::Receiver<SinkEvent>,
    mut resolved_rx: mpsc::Receiver<TerminalOutcome>,
    state: Arc<Mutex<InteractState>>,
    action_tx: mpsc::Sender<DispatchOrder>,
    props_events: PropsEventIngress,
) {
    loop {
        tokio::select! {
            biased;
            event = terminal_sig_rx.recv() => {
                let Some(event) = event else { return };
                let (handle, revision, signal) = match event {
                    SinkEvent::Closed { handle, revision, fd_id, expired } => (
                        handle,
                        revision,
                        DeliverySignal::Closed {
                            fd_id,
                            kind: if expired { ClosedKind::Expired } else { ClosedKind::Dismissed },
                        },
                    ),
                    SinkEvent::Action { .. } => continue,
                };
                let result = {
                    let mut state = state.lock().expect("interact state poisoned");
                    let result = state.on_sink_signal(&handle, revision, signal);
                    let transitions = drain_props_events(&mut state);
                    props_events.enqueue(transitions);
                    result
                };
                if let SignalResult::Resolved(outcome) = result {
                    handle_terminal_outcome(outcome, &action_tx);
                }
            }
            outcome = resolved_rx.recv() => {
                let Some(outcome) = outcome else { return };
                handle_terminal_outcome(outcome, &action_tx);
            }
            event = action_sig_rx.recv() => {
                let Some(event) = event else { return };
                let SinkEvent::Action { handle, revision, fd_id, key } = event else {
                    continue;
                };
                let result = {
                    let mut state = state.lock().expect("interact state poisoned");
                    let result = state.on_sink_signal(
                        &handle,
                        revision,
                        DeliverySignal::Action { fd_id, key },
                    );
                    let transitions = drain_props_events(&mut state);
                    props_events.enqueue(transitions);
                    result
                };
                if let SignalResult::Resolved(outcome) = result {
                    handle_terminal_outcome(outcome, &action_tx);
                }
            }
        }
    }
}

async fn fail_delivery(
    sink: &mut dyn NotifySink,
    state: &Arc<Mutex<InteractState>>,
    job: &DeliveryJob,
    reason: &str,
    props_events: &PropsEventIngress,
) {
    let current = {
        let mut state = state.lock().expect("interact state poisoned");
        let current = state.complete_delivery(job, DeliveryCompletion::Failed);
        let transitions = drain_props_events(&mut state);
        props_events.enqueue(transitions);
        current
    };
    if current.current {
        eprintln!(
            "cosmix-interactd: [notify] delivery failed for {} ({reason}); recorded as failed",
            job.handle.as_str()
        );
        close_and_retire(sink, &job.handle).await;
    }
}

async fn close_and_retire(sink: &mut dyn NotifySink, handle: &NotifyHandle) {
    match tokio::time::timeout(IO_TIMEOUT, sink.close(handle)).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => eprintln!(
            "cosmix-interactd: [notify] close failed for {}: {error}",
            handle.as_str()
        ),
        Err(_) => eprintln!(
            "cosmix-interactd: [notify] close timed out for {}",
            handle.as_str()
        ),
    }
    // `close` implementations remove their retained entry before awaiting the
    // backend. `retire` is the unconditional local cleanup backstop.
    sink.retire(handle);
}

/// Single bounded delivery worker. It serialises registry snapshots and sink
/// mutations, caps each external wait, and never holds the state mutex over an
/// await. Request handlers communicate with it exclusively through the channel.
async fn run_delivery(
    mut delivery_rx: mpsc::Receiver<DeliveryCommand>,
    mut sink: Box<dyn NotifySink>,
    state: Arc<Mutex<InteractState>>,
    resolved_tx: mpsc::Sender<TerminalOutcome>,
    props_events: PropsEventIngress,
) {
    let mut tombstones = JoinSet::<(DeliveryJob, TombstoneResolution)>::new();
    let mut delivery_open = true;
    while delivery_open || !tombstones.is_empty() {
        tokio::select! {
        resolved = tombstones.join_next(), if !tombstones.is_empty() => {
            if let Some(Ok((job, resolution))) = resolved {
                let reason = match resolution {
                    TombstoneResolution::NoHandle => {
                        "ambiguous notification delivery resolved without a late handle"
                    }
                    TombstoneResolution::Closed => {
                        "ambiguous notification delivery resolved and late handle was closed"
                    }
                    TombstoneResolution::CloseTimedOut => {
                        "ambiguous notification delivery resolved; late handle close timed out"
                    }
                };
                fail_delivery(
                    &mut *sink,
                    &state,
                    &job,
                    reason,
                    &props_events,
                )
                .await;
            }
        }
        command = delivery_rx.recv(), if delivery_open => {
        let Some(command) = command else {
            delivery_open = false;
            continue;
        };
        match command {
            DeliveryCommand::Show {
                job,
                registry_client,
                _bytes: _,
            } => {
                let current = state
                    .lock()
                    .expect("interact state poisoned")
                    .is_current(&job);
                if !current {
                    continue;
                }

                if job
                    .request
                    .actions
                    .iter()
                    .any(|action| action.on_invoke.is_some())
                {
                    let Some(client) = registry_client else {
                        fail_delivery(
                            &mut *sink,
                            &state,
                            &job,
                            "service registry unavailable",
                            &props_events,
                        )
                        .await;
                        continue;
                    };
                    let services =
                        match tokio::time::timeout(IO_TIMEOUT, client.list_services()).await {
                            Ok(Ok(services)) => services.into_iter().collect::<HashSet<_>>(),
                            Ok(Err(error)) => {
                                fail_delivery(
                                    &mut *sink,
                                    &state,
                                    &job,
                                    &format!("service registry query failed: {error}"),
                                    &props_events,
                                )
                                .await;
                                continue;
                            }
                            Err(_) => {
                                fail_delivery(
                                    &mut *sink,
                                    &state,
                                    &job,
                                    "service registry query timed out",
                                    &props_events,
                                )
                                .await;
                                continue;
                            }
                        };
                    if let Err(reason) = validate_dispatch_targets(&job.request, &|name: &str| {
                        services.contains(name)
                    }) {
                        let reason = match reason {
                            RejectReason::UnregisteredDispatch { service, .. } => {
                                format!("dispatch target not registered: {service}")
                            }
                            RejectReason::Invalid(error) => {
                                format!("queued request became invalid: {error}")
                            }
                        };
                        fail_delivery(&mut *sink, &state, &job, &reason, &props_events).await;
                        continue;
                    }
                }

                if tombstones.len() >= AMBIGUOUS_SHOW_CAP {
                    fail_delivery(
                        &mut *sink,
                        &state,
                        &job,
                        "ambiguous-delivery tombstone capacity exhausted",
                        &props_events,
                    )
                    .await;
                    continue;
                }
                let view = job.view();
                match sink
                    .show(&job.handle, job.revision, &view, job.replaces.as_ref())
                    .await
                {
                    Ok(ShowOutcome::Shown(fd_id)) => {
                        let completion = {
                            let mut state = state.lock().expect("interact state poisoned");
                            let completion =
                                state.complete_delivery(&job, DeliveryCompletion::Shown(fd_id));
                            let transitions = drain_props_events(&mut state);
                            props_events.enqueue(transitions);
                            completion
                        };
                        if let Some(outcome) = completion.terminal {
                            sink.retire(&job.handle);
                            if resolved_tx.send(outcome).await.is_err() {
                                return;
                            }
                        } else if !completion.current {
                            let terminal = state
                                .lock()
                                .expect("interact state poisoned")
                                .records()
                                .get(job.handle.as_str())
                                .is_none_or(|record| record.state.is_terminal());
                            if terminal {
                                close_and_retire(&mut *sink, &job.handle).await;
                            }
                        }
                    }
                    Ok(ShowOutcome::Ambiguous(tombstone)) => {
                        eprintln!(
                            "cosmix-interactd: [notify] delivery ambiguous for {}; keeping queued and dedupe-owned until tombstone resolves",
                            job.handle.as_str()
                        );
                        tombstones.spawn(async move {
                            let resolution = tombstone.resolve().await;
                            (*job, resolution)
                        });
                    }
                    Err(error) => {
                        fail_delivery(
                            &mut *sink,
                            &state,
                            &job,
                            &error.to_string(),
                            &props_events,
                        )
                        .await;
                    }
                }
            }
            DeliveryCommand::Close { handle, _bytes: _ } => {
                close_and_retire(&mut *sink, &handle).await;
            }
        }
        }
        }
    }
}

/// The command loop for one live connection. Locks the shared state only to
/// compute a response — the guard is dropped before the (async) reply so it is
/// never held across an `.await`. `interact.notify` only records and enqueues a
/// delivery; registry lookup, D-Bus dispatch and action-listener setup all run
/// later on the delivery worker.
async fn serve_bus(
    client: Arc<NodedClient>,
    state: &Arc<Mutex<InteractState>>,
    delivery_tx: &mpsc::Sender<DeliveryCommand>,
    delivery_bytes: &DeliveryByteBudget,
    props_events: &PropsEventIngress,
    dialog_maintenance_tx: &watch::Sender<Option<u64>>,
) {
    let Some(mut rx) = client.incoming_async().await else {
        return;
    };
    while let Some(cmd) = rx.recv().await {
        let (rc, body) = match authorize_caller(&cmd) {
            Ok(caller) if cmd.command == "interact.notify" => handle_notify(
                &client,
                state,
                &cmd,
                &caller,
                delivery_tx,
                delivery_bytes,
                props_events,
            ),
            Ok(caller) => {
                let mut st = state.lock().expect("interact state poisoned");
                let response = dispatch(&mut st, &cmd, &caller, delivery_tx, delivery_bytes);
                let transitions = drain_props_events(&mut st);
                reschedule_dialog_maintenance(&st, dialog_maintenance_tx);
                props_events.enqueue(transitions);
                response
            }
            Err(error) => app_error(error.id, &error.message),
        };
        let _ = client
            .respond_parts(&cmd.from, &cmd.command, cmd.id.as_deref(), rc, &body)
            .await;
    }
}

/// `interact.notify`: parse, reserve bounded delivery capacity, record `Queued`,
/// and return. Registry lookup and desktop delivery occur later on the worker.
fn handle_notify(
    client: &Arc<NodedClient>,
    state: &Arc<Mutex<InteractState>>,
    cmd: &IncomingCommand,
    caller: &AuthorizedCaller,
    delivery_tx: &mpsc::Sender<DeliveryCommand>,
    delivery_bytes: &DeliveryByteBudget,
    props_events: &PropsEventIngress,
) -> (u8, String) {
    let args = match resolve_bounded_args(cmd) {
        Ok(args) => args.unwrap_or(Value::Null),
        Err(response) => return response,
    };
    let create = match serde_json::from_value::<NotifyCreateRequest>(args) {
        Ok(create) => create,
        Err(e) => return err(&format!("bad notify args: {e}")),
    };
    let req = create.request;
    if let Err(message) = validate_icon_policy(req.icon.as_ref()) {
        return app_error("unsupported", &message);
    }
    if let Err(error) = req.validate() {
        return app_error(error.code(), &format!("invalid notify: {error}"));
    }
    let permit = match delivery_tx.try_reserve() {
        Ok(permit) => permit,
        Err(_) => {
            return app_error(
                "delivery_queue_full",
                "notify.v1 delivery queue is full; request was not accepted",
            );
        }
    };
    let Some(byte_permit) = delivery_bytes.try_reserve(delivery_job_bytes(&req)) else {
        return app_error(
            "delivery_queue_full",
            "notify.v1 delivery queue byte budget is full; request was not accepted",
        );
    };

    let (rc, body, job) = {
        let mut st = state.lock().expect("interact state poisoned");
        let (rc, body, job) = st.notify(
            req,
            caller.broker_caller(),
            NotifyOwnership::new(caller.owner.clone(), create.owner_token, mint_owner_token()),
            now_ms(),
            mint_handle(),
        );
        let transitions = drain_props_events(&mut st);
        props_events.enqueue(transitions);
        (rc, body, job)
    };
    if let Some(job) = job {
        permit.send(DeliveryCommand::Show {
            job: Box::new(job),
            registry_client: Some(client.clone()),
            _bytes: byte_permit,
        });
    }
    (rc, body)
}

fn validate_icon_policy(icon: Option<&IconRef>) -> Result<(), String> {
    match icon {
        Some(IconRef::Emoji(key)) => Err(format!(
            "notify.v1 emoji icon {key:?} is unsupported until the shared emoji catalogue ships"
        )),
        Some(IconRef::Lucide(key)) if sink::resolve_icon(icon).is_none() => Err(format!(
            "notify.v1 Lucide icon {key:?} is not in the supported catalogue"
        )),
        Some(IconRef::Lucide(_)) | None => Ok(()),
    }
}

/// Route one command to a verb handler. Returns `(rc, json-body)`.
fn dispatch(
    state: &mut InteractState,
    cmd: &IncomingCommand,
    caller: &AuthorizedCaller,
    delivery_tx: &mpsc::Sender<DeliveryCommand>,
    delivery_bytes: &DeliveryByteBudget,
) -> (u8, String) {
    dispatch_at(state, cmd, caller, delivery_tx, delivery_bytes, now_ms())
}

fn dispatch_at(
    state: &mut InteractState,
    cmd: &IncomingCommand,
    caller: &AuthorizedCaller,
    delivery_tx: &mpsc::Sender<DeliveryCommand>,
    delivery_bytes: &DeliveryByteBudget,
    command_now_ms: u64,
) -> (u8, String) {
    if let Some(suffix) = cmd.command.strip_prefix("interact.props.") {
        if suffix == "watch" {
            return (
                0,
                json!({
                    "topic": PROPS_CHANGED_TOPIC,
                    "queue_capacity": PROPS_EVENT_QUEUE_CAP,
                    "event_sequence": "daemon_session_monotonic",
                    "event_seq": state.props_event_seq(),
                    "loss_signal": "gap_and_lost_count",
                    "bootstrap": "subscribe to the topic on this connection, then read interact.props.get",
                })
                .to_string(),
            );
        }
        let args = resolve_args(cmd);
        let props = InteractionsProps::from_records(
            state.records(),
            state.dialog_records(),
            state.props_event_seq(),
        );
        let resp = cosmix_props_core::bus::dispatch_props(&props, suffix, args.as_ref(), true);
        return (resp.rc.clamp(0, 255) as u8, resp.body);
    }

    match cmd.command.as_str() {
        // `interact.notify` is handled on the async path (see `handle_notify`);
        // it never reaches this synchronous router.
        VERB_DIALOG_OPEN => {
            let args = match resolve_dialog_args(cmd) {
                Ok(args) => args.unwrap_or(Value::Null),
                Err(response) => return response,
            };
            let request = match serde_json::from_value::<DialogOpenRequestV1>(args) {
                Ok(request) => request,
                Err(error) => return err(&format!("bad dialog-open args: {error}")),
            };
            if let Err(error) = request.validate() {
                return app_error("invalid_dialog", &format!("invalid dialog-open: {error}"));
            }
            match state.dialog_open(
                &caller.origin,
                mint_owner_token(),
                request,
                command_now_ms,
                mint_dialog_handle(),
            ) {
                Ok(response) => json_response(&response),
                Err(error) => dialog_error_response(error),
            }
        }
        VERB_DIALOG_PROGRESS_UPDATE => {
            let args = match resolve_dialog_args(cmd) {
                Ok(args) => args.unwrap_or(Value::Null),
                Err(response) => return response,
            };
            if let Some(response) = require_dialog_owner_fields(&args, VERB_DIALOG_PROGRESS_UPDATE)
            {
                return response;
            }
            let request = match serde_json::from_value::<DialogProgressUpdateRequest>(args) {
                Ok(request) => request,
                Err(error) => return err(&format!("bad dialog-progress-update args: {error}")),
            };
            if let Err(error) = request.patch.validate() {
                return app_error(
                    "invalid_dialog",
                    &format!("invalid dialog-progress-update: {error}"),
                );
            }
            match state.dialog_progress_update(
                &caller.origin,
                &request.owner_token,
                &request.handle,
                request.patch,
                command_now_ms,
            ) {
                Ok(()) => empty_success(),
                Err(error) => dialog_error_response(error),
            }
        }
        VERB_DIALOG_PROGRESS_COMPLETE => {
            let args = match resolve_dialog_args(cmd) {
                Ok(args) => args.unwrap_or(Value::Null),
                Err(response) => return response,
            };
            if let Some(response) =
                require_dialog_owner_fields(&args, VERB_DIALOG_PROGRESS_COMPLETE)
            {
                return response;
            }
            let request = match serde_json::from_value::<DialogProgressCompleteRequest>(args) {
                Ok(request) => request,
                Err(error) => {
                    return err(&format!("bad dialog-progress-complete args: {error}"));
                }
            };
            if let Err(error) = request.completion.validate() {
                return app_error(
                    "invalid_dialog",
                    &format!("invalid dialog-progress-complete: {error}"),
                );
            }
            match state.dialog_progress_complete(
                &caller.origin,
                &request.owner_token,
                &request.handle,
                request.completion,
                command_now_ms,
            ) {
                Ok(()) => empty_success(),
                Err(error) => dialog_error_response(error),
            }
        }
        VERB_DIALOG_CANCEL => {
            let args = match resolve_dialog_args(cmd) {
                Ok(args) => args.unwrap_or(Value::Null),
                Err(response) => return response,
            };
            if let Some(response) = require_dialog_owner_fields(&args, VERB_DIALOG_CANCEL) {
                return response;
            }
            let request = match serde_json::from_value::<DialogCancelRequest>(args) {
                Ok(request) => request,
                Err(error) => return err(&format!("bad dialog-cancel args: {error}")),
            };
            match state.dialog_cancel(
                &caller.origin,
                &request.owner_token,
                &request.handle,
                command_now_ms,
            ) {
                Ok(()) => empty_success(),
                Err(error) => dialog_error_response(error),
            }
        }
        VERB_DIALOG_RESULT => {
            let args = match resolve_dialog_args(cmd) {
                Ok(args) => args.unwrap_or(Value::Null),
                Err(response) => return response,
            };
            if let Some(response) = require_dialog_owner_fields(&args, VERB_DIALOG_RESULT) {
                return response;
            }
            let request = match serde_json::from_value::<DialogResultRequest>(args) {
                Ok(request) => request,
                Err(error) => return err(&format!("bad dialog-result args: {error}")),
            };
            match state.dialog_result(
                &caller.origin,
                &request.owner_token,
                &request.handle,
                command_now_ms,
            ) {
                Ok(response) => json_response(&response),
                Err(error) => dialog_error_response(error),
            }
        }
        VERB_PRESENTER_REGISTER => {
            if let Some(response) = require_dialog_presenter(caller) {
                return response;
            }
            let args = match resolve_dialog_args(cmd) {
                Ok(args) => args.unwrap_or(Value::Null),
                Err(response) => return response,
            };
            if let Err(error) = serde_json::from_value::<DialogPresenterRegisterRequestV1>(args) {
                return err(&format!("bad presenter-register args: {error}"));
            }
            match state.presenter_register(&caller.origin, command_now_ms) {
                Ok(response) => json_response(&response),
                Err(error) => dialog_error_response(error),
            }
        }
        VERB_PRESENTER_RELEASE => {
            if let Some(response) = require_dialog_presenter(caller) {
                return response;
            }
            let args = match resolve_dialog_args(cmd) {
                Ok(args) => args.unwrap_or(Value::Null),
                Err(response) => return response,
            };
            let request = match serde_json::from_value::<DialogPresenterReleaseRequestV1>(args) {
                Ok(request) => request,
                Err(error) => return err(&format!("bad presenter-release args: {error}")),
            };
            match state.presenter_release(&request.lease, command_now_ms) {
                Ok(()) => empty_success(),
                Err(error) => dialog_error_response(error),
            }
        }
        VERB_PRESENTER_NEXT => {
            if let Some(response) = require_dialog_presenter(caller) {
                return response;
            }
            let args = match resolve_dialog_args(cmd) {
                Ok(args) => args.unwrap_or(Value::Null),
                Err(response) => return response,
            };
            let request = match serde_json::from_value::<DialogPresenterNextRequestV1>(args) {
                Ok(request) => request,
                Err(error) => return err(&format!("bad presenter-next args: {error}")),
            };
            match state.presenter_next(&request.lease, command_now_ms) {
                Ok(response) => json_response(&response),
                Err(error) => dialog_error_response(error),
            }
        }
        VERB_PRESENTER_MARK_PRESENTED => {
            if let Some(response) = require_dialog_presenter(caller) {
                return response;
            }
            let args = match resolve_dialog_args(cmd) {
                Ok(args) => args.unwrap_or(Value::Null),
                Err(response) => return response,
            };
            let request =
                match serde_json::from_value::<DialogPresenterMarkPresentedRequestV1>(args) {
                    Ok(request) => request,
                    Err(error) => {
                        return err(&format!("bad presenter-mark-presented args: {error}"));
                    }
                };
            match state.presenter_mark_presented(
                &request.lease,
                &InteractionHandle(request.handle),
                request.attempt_token,
                command_now_ms,
            ) {
                Ok(()) => empty_success(),
                Err(error) => dialog_error_response(error),
            }
        }
        VERB_PRESENTER_RESOLVE => {
            if let Some(response) = require_dialog_presenter(caller) {
                return response;
            }
            let args = match resolve_dialog_args(cmd) {
                Ok(args) => args.unwrap_or(Value::Null),
                Err(response) => return response,
            };
            let request = match serde_json::from_value::<DialogPresenterResolveRequestV1>(args) {
                Ok(request) => request,
                Err(error) => return err(&format!("bad presenter-resolve args: {error}")),
            };
            match state.presenter_resolve(
                &request.lease,
                &InteractionHandle(request.handle),
                request.attempt_token,
                request.value,
                command_now_ms,
            ) {
                Ok(()) => empty_success(),
                Err(error) => dialog_error_response(error),
            }
        }
        VERB_PRESENTER_FAIL => {
            if let Some(response) = require_dialog_presenter(caller) {
                return response;
            }
            let args = match resolve_dialog_args(cmd) {
                Ok(args) => args.unwrap_or(Value::Null),
                Err(response) => return response,
            };
            let request = match serde_json::from_value::<DialogPresenterFailRequestV1>(args) {
                Ok(request) => request,
                Err(error) => return err(&format!("bad presenter-fail args: {error}")),
            };
            match state.presenter_fail(
                &request.lease,
                &InteractionHandle(request.handle),
                request.attempt_token,
                command_now_ms,
            ) {
                Ok(()) => empty_success(),
                Err(error) => dialog_error_response(error),
            }
        }
        VERB_PRESENTER_PROGRESS_CANCEL => {
            if let Some(response) = require_dialog_presenter(caller) {
                return response;
            }
            let args = match resolve_dialog_args(cmd) {
                Ok(args) => args.unwrap_or(Value::Null),
                Err(response) => return response,
            };
            let request =
                match serde_json::from_value::<DialogPresenterProgressCancelRequestV1>(args) {
                    Ok(request) => request,
                    Err(error) => {
                        return err(&format!("bad presenter-progress-cancel args: {error}"));
                    }
                };
            match state.presenter_progress_cancel(
                &request.lease,
                &InteractionHandle(request.handle),
                request.attempt_token,
                command_now_ms,
            ) {
                Ok(()) => empty_success(),
                Err(error) => dialog_error_response(error),
            }
        }
        "interact.update" => {
            let args = match resolve_bounded_args(cmd) {
                Ok(args) => args.unwrap_or(Value::Null),
                Err(response) => return response,
            };
            if args.get("handle").and_then(Value::as_str).is_none() {
                return err("interact.update requires a handle");
            }
            if args.get("owner_token").and_then(Value::as_str).is_none() {
                return app_error(
                    "owner_token_required",
                    "interact.update requires the owner_token returned by interact.notify",
                );
            }
            match serde_json::from_value::<NotifyUpdateRequest>(args) {
                Ok(update) => {
                    if let Err(message) = validate_icon_policy(update.request.icon.as_ref()) {
                        return app_error("unsupported", &message);
                    }
                    if let Err(error) = update.request.validate() {
                        return app_error(error.code(), &format!("invalid update: {error}"));
                    }
                    let permit = match delivery_tx.try_reserve() {
                        Ok(permit) => permit,
                        Err(_) => {
                            return app_error(
                                "delivery_queue_full",
                                "notify.v1 delivery queue is full; update was not accepted",
                            );
                        }
                    };
                    let Some(byte_permit) =
                        delivery_bytes.try_reserve(delivery_job_bytes(&update.request))
                    else {
                        return app_error(
                            "delivery_queue_full",
                            "notify.v1 delivery queue byte budget is full; update was not accepted",
                        );
                    };
                    let (rc, body, job) = state.update(
                        &caller.owner,
                        &update.owner_token,
                        update.handle.as_str(),
                        update.request,
                        command_now_ms,
                    );
                    if let Some(job) = job {
                        permit.send(DeliveryCommand::Show {
                            job: Box::new(job),
                            registry_client: None,
                            _bytes: byte_permit,
                        });
                    }
                    (rc, body)
                }
                Err(e) => err(&format!("bad update args: {e}")),
            }
        }
        "interact.dismiss" => {
            let args = match resolve_bounded_args(cmd) {
                Ok(args) => args.unwrap_or(Value::Null),
                Err(response) => return response,
            };
            if args.get("handle").and_then(Value::as_str).is_none() {
                return err("interact.dismiss requires a handle");
            }
            if args.get("owner_token").and_then(Value::as_str).is_none() {
                return app_error(
                    "owner_token_required",
                    "interact.dismiss requires the owner_token returned by interact.notify",
                );
            }
            match serde_json::from_value::<NotifyDismissRequest>(args) {
                Ok(dismiss) => {
                    let permit = match delivery_tx.try_reserve() {
                        Ok(permit) => permit,
                        Err(_) => {
                            return app_error(
                                "delivery_queue_full",
                                "notify.v1 delivery queue is full; dismissal was not accepted",
                            );
                        }
                    };
                    let Some(byte_permit) =
                        delivery_bytes.try_reserve(delivery_close_bytes(&dismiss.handle))
                    else {
                        return app_error(
                            "delivery_queue_full",
                            "notify.v1 delivery queue byte budget is full; dismissal was not accepted",
                        );
                    };
                    let (rc, body, handle) =
                        state.dismiss(&caller.owner, &dismiss.owner_token, dismiss.handle.as_str());
                    if let Some(handle) = handle {
                        permit.send(DeliveryCommand::Close {
                            handle,
                            _bytes: byte_permit,
                        });
                    }
                    (rc, body)
                }
                Err(e) => err(&format!("bad dismiss args: {e}")),
            }
        }
        other => err(&format!("unknown verb: {other}")),
    }
}

fn validate_wire_request_size(bytes: usize) -> Result<(), (u8, String)> {
    validate_wire_request_size_with_cap(bytes, MAX_REQUEST_BYTES, "notify")
}

fn validate_dialog_wire_request_size(bytes: usize) -> Result<(), (u8, String)> {
    validate_wire_request_size_with_cap(bytes, DIALOG_RAW_INGRESS_CAP, "dialog")
}

fn validate_wire_request_size_with_cap(
    bytes: usize,
    cap: usize,
    surface: &str,
) -> Result<(), (u8, String)> {
    if bytes > cap {
        Err(app_error(
            cosmix_interaction_schema::VALIDATION_REQUEST_TOO_LARGE,
            &format!("{surface} request wire payload is too large: {bytes} bytes > {cap}"),
        ))
    } else {
        Ok(())
    }
}

/// Resolve notification arguments without estimating raw input from parsed
/// JSON. The body is pre-checked before any parsing here. A valid `args` header
/// wins and adds its raw bytes to the body; an invalid header is not consumed,
/// so fallback counts only the raw body represented by `cmd.args`.
fn resolve_bounded_args(cmd: &IncomingCommand) -> Result<Option<Value>, (u8, String)> {
    resolve_bounded_args_with(cmd, validate_wire_request_size)
}

fn resolve_dialog_args(cmd: &IncomingCommand) -> Result<Option<Value>, (u8, String)> {
    resolve_bounded_args_with(cmd, validate_dialog_wire_request_size)
}

fn resolve_bounded_args_with(
    cmd: &IncomingCommand,
    validate_size: fn(usize) -> Result<(), (u8, String)>,
) -> Result<Option<Value>, (u8, String)> {
    validate_size(cmd.body.len())?;

    if let Some(header) = cmd.header("args") {
        validate_size(header.len())?;
        if let Ok(value) = serde_json::from_str::<Value>(header) {
            validate_size(cmd.body.len().saturating_add(header.len()))?;
            return Ok(Some(value));
        }
    }
    if !cmd.args.is_null() {
        return Ok(Some(cmd.args.clone()));
    }
    if !cmd.body.is_empty()
        && let Ok(value) = serde_json::from_str::<Value>(&cmd.body)
    {
        return Ok(Some(value));
    }
    Ok(None)
}

fn require_dialog_owner_fields(args: &Value, verb: &str) -> Option<(u8, String)> {
    if args.get("handle").and_then(Value::as_str).is_none() {
        return Some(err(&format!("{verb} requires a handle")));
    }
    if args.get("owner_token").and_then(Value::as_str).is_none() {
        return Some(app_error(
            "owner_token_required",
            &format!("{verb} requires the owner_token returned by interact.dialog-open"),
        ));
    }
    None
}

fn require_dialog_presenter(caller: &AuthorizedCaller) -> Option<(u8, String)> {
    (caller.origin != "interact-gui").then(|| {
        app_error(
            "presenter_forbidden",
            "dialog presenter verbs require the authenticated interact-gui service",
        )
    })
}

fn json_response<T: serde::Serialize>(response: &T) -> (u8, String) {
    (
        0,
        serde_json::to_string(response).expect("interaction response is serializable"),
    )
}

fn empty_success() -> (u8, String) {
    (0, "{}".to_string())
}

fn dialog_error_response(error: DialogBrokerError) -> (u8, String) {
    let id = match &error {
        DialogBrokerError::Invalid(_) => "invalid_dialog",
        DialogBrokerError::RateLimited => "dialog_rate_limited",
        DialogBrokerError::QueueFull { .. } => "dialog_queue_full",
        DialogBrokerError::OriginQueueFull { .. } => "dialog_origin_queue_full",
        DialogBrokerError::DuplicateHandle => "duplicate_handle",
        DialogBrokerError::NotFound => "dialog_not_found",
        DialogBrokerError::WrongOwner => "ownership_denied",
        DialogBrokerError::NoPresenter => "no_presenter",
        DialogBrokerError::StaleLease => "stale_lease",
        DialogBrokerError::StaleAttempt => "stale_attempt",
        DialogBrokerError::CounterExhausted => "counter_exhausted",
        DialogBrokerError::AlreadyTerminal => "already_terminal",
        DialogBrokerError::Expired => "dialog_expired",
        DialogBrokerError::InvalidState(_) => "invalid_state",
        DialogBrokerError::OwnerCannotResolve => "owner_cannot_resolve",
        DialogBrokerError::ProgressPresenterResolution => "progress_presenter_resolution",
        DialogBrokerError::NotProgress => "not_progress",
        DialogBrokerError::NotCancellable => "not_cancellable",
    };
    app_error(id, &error.to_string())
}

fn delivery_job_bytes(request: &NotifyRequest) -> usize {
    std::mem::size_of::<DeliveryJob>().saturating_add(request.retained_bytes())
}

fn delivery_close_bytes(handle: &NotifyHandle) -> usize {
    std::mem::size_of::<NotifyHandle>().saturating_add(handle.as_str().len())
}

/// Admit only traffic whose current provenance can be described honestly.
/// `source_peer` and `permissions` are wire-supplied today, so neither is trust
/// evidence and any request carrying either is rejected. This gate may lift
/// only after noded supplies a non-wire-assertable authenticated `signed_ident`;
/// interactd must then resolve grants through mesh-trust's
/// `resolve_cross_mesh_caps` path and the exposable allowlist, requiring
/// [`CTK_NOTIFY`]. Only a non-empty, canonical noded-rewritten local service
/// name is admitted; empty/invalid `from` covers anonymous local connections
/// and today's mesh ingress, neither of which has notify.v1 authority.
fn authorize_caller(cmd: &IncomingCommand) -> Result<AuthorizedCaller, AuthorizationError> {
    let carries_wire_auth_claim = cmd.headers.keys().any(|name| {
        name.eq_ignore_ascii_case("source_peer")
            || name.eq_ignore_ascii_case("permissions")
            || name.eq_ignore_ascii_case("signed_ident")
    });
    if carries_wire_auth_claim {
        return Err(AuthorizationError {
            id: "remote_identity_unavailable",
            message: format!(
                "remote ingress is closed until noded supplies authenticated signed_ident provenance for mesh-trust {CTK_NOTIFY} grant resolution"
            ),
        });
    }

    // Locality gate for interact.* verbs (notify + update + dialog-* +
    // presenter-*): interactd's entire action surface. noded stamps
    // `broker_origin` from the source socket on every routed delivery, stripping
    // any client spelling first, so a present-and-`local` stamp is the only
    // trustworthy proof of node-local origin — `from` canonicality cannot
    // distinguish a mesh peer presenting `from="interact-gui"` from the real
    // local presenter. Mesh-routed and unattributed interact.* traffic is
    // rejected here; the presenter/owner verbs must never be driven off-node.
    // Scoped to `interact.*` so noded-internal topic lifecycle notices
    // (`topic.active`/`topic.idle`, always local, delivered unstamped) are
    // unaffected.
    if cmd.command.starts_with("interact.") {
        let node_local = cmd.headers.iter().any(|(name, value)| {
            name.eq_ignore_ascii_case(BROKER_ORIGIN_HEADER) && value == BROKER_ORIGIN_LOCAL
        });
        if !node_local {
            return Err(AuthorizationError {
                id: "remote_ingress_closed",
                message: "interact.* verbs require node-local origin (broker_origin=local); mesh-routed and unattributed ingress are rejected".into(),
            });
        }
    }

    if !is_bus_service_name(&cmd.from) {
        return Err(AuthorizationError {
            id: "unregistered_caller",
            message: "notify.v1 requires a canonically registered local noded service; anonymous and unauthenticated mesh ingress are rejected".into(),
        });
    }

    Ok(AuthorizedCaller {
        owner: OwnerId::local(&cmd.from),
        origin: cmd.from.clone(),
    })
}

/// Parse a command's args: the `args` header (JSON) wins, then a non-null
/// `cmd.args`, then the raw body as JSON. `None` if none present. (Same
/// precedence as filesd.)
fn resolve_args(cmd: &IncomingCommand) -> Option<Value> {
    if let Some(h) = cmd.header("args")
        && let Ok(v) = serde_json::from_str::<Value>(h)
    {
        return Some(v);
    }
    if !cmd.args.is_null() {
        return Some(cmd.args.clone());
    }
    if !cmd.body.is_empty()
        && let Ok(v) = serde_json::from_str::<Value>(&cmd.body)
    {
        return Some(v);
    }
    None
}

/// Mint a fresh opaque handle. A `uuid` simple form (32 hex, no separators) is a
/// valid single props-path segment and unguessable — never derived from request
/// contents (notify.v1 §7).
fn mint_handle() -> NotifyHandle {
    NotifyHandle(format!("n{}", uuid::Uuid::new_v4().simple()))
}

fn mint_dialog_handle() -> InteractionHandle {
    InteractionHandle(format!("d{}", uuid::Uuid::new_v4().simple()))
}

/// Mint the bearer capability required for every later mutation or dedupe
/// replacement. It is independent of the public handle and never enters props.
fn mint_owner_token() -> OwnerToken {
    OwnerToken(format!("o{}", uuid::Uuid::new_v4().simple()))
}

/// Wall-clock ms since the Unix epoch. The broker clamps backwards movement to
/// each bucket's high-water mark, so NTP correction cannot mint refill credit.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A uniform error response: rc 10 + `{ "error": msg }`.
pub(crate) fn err(msg: &str) -> (u8, String) {
    (10, json!({ "error": msg }).to_string())
}

fn app_error(id: &str, message: &str) -> (u8, String) {
    (
        10,
        json!({ "error": { "id": id, "message": message } }).to_string(),
    )
}

pub(crate) fn app_error_with_handle(id: &str, message: &str, handle: &str) -> (u8, String) {
    (
        10,
        json!({ "error": { "id": id, "message": message }, "handle": handle }).to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeMap;
    use std::num::NonZeroU64;

    use async_trait::async_trait;
    use cosmix_interaction_broker::MAX_DIALOG_PRE_DISPLAY_REQUEUES;
    use cosmix_interaction_schema::{
        DialogPresenterLeaseV1, DialogStateV1, DialogValueV1, MIN_DIALOG_DEADLINE_MS, NotifyState,
        Urgency,
    };

    fn test_byte_permit() -> DeliveryBytePermit {
        DeliveryByteBudget::new(MAX_DELIVERY_QUEUE_BYTES)
            .try_reserve(1)
            .unwrap()
    }
    fn command(
        from: &str,
        source_peer: Option<&str>,
        permissions: Option<&str>,
    ) -> IncomingCommand {
        let mut headers = BTreeMap::new();
        // Simulate noded's routed-delivery stamp for a node-local source; the
        // locality-rejection tests override this to `mesh`/absent.
        headers.insert(BROKER_ORIGIN_HEADER.into(), BROKER_ORIGIN_LOCAL.into());
        if let Some(peer) = source_peer {
            headers.insert("source_peer".into(), peer.into());
        }
        if let Some(caps) = permissions {
            headers.insert("permissions".into(), caps.into());
        }
        IncomingCommand {
            from: from.into(),
            command: "interact.notify".into(),
            id: Some("test-1".into()),
            args: Value::Null,
            body: String::new(),
            headers,
        }
    }

    fn wire_call_at(
        state: &mut InteractState,
        from: &str,
        verb: &str,
        args: Value,
        at_ms: u64,
    ) -> (u8, Value, Vec<PropsEvent>) {
        let mut cmd = command(from, None, None);
        cmd.command = verb.to_string();
        cmd.args = args;
        let (delivery_tx, _delivery_rx) = mpsc::channel(4);
        let budget = DeliveryByteBudget::new(MAX_DELIVERY_QUEUE_BYTES);
        let response = match authorize_caller(&cmd) {
            Ok(caller) => dispatch_at(state, &cmd, &caller, &delivery_tx, &budget, at_ms),
            Err(error) => app_error(error.id, &error.message),
        };
        let events = drain_props_events(state);
        (
            response.0,
            serde_json::from_str(&response.1).unwrap(),
            events,
        )
    }

    fn message_dialog(title: &str, deadline_ms: Option<u64>) -> Value {
        json!({
            "dialog": {
                "kind": "message",
                "common": {
                    "title": title,
                    "message": "body",
                    "severity": "info"
                }
            },
            "deadline_ms": deadline_ms
        })
    }

    fn progress_dialog(message: &str) -> Value {
        json!({
            "dialog": {
                "kind": "progress",
                "common": {
                    "title": "Work",
                    "message": message,
                    "severity": "info"
                },
                "progress": {
                    "mode": "determinate",
                    "current": 0,
                    "total": 10
                },
                "cancellable": true
            }
        })
    }

    fn open_dialog(
        state: &mut InteractState,
        owner: &str,
        args: Value,
        at_ms: u64,
    ) -> (String, String, Vec<PropsEvent>) {
        let (rc, body, events) = wire_call_at(state, owner, VERB_DIALOG_OPEN, args, at_ms);
        assert_eq!(rc, 0, "{body}");
        (
            body["handle"].as_str().unwrap().to_string(),
            body["owner_token"].as_str().unwrap().to_string(),
            events,
        )
    }

    fn register_presenter(
        state: &mut InteractState,
        at_ms: u64,
    ) -> (DialogPresenterLeaseV1, Vec<PropsEvent>) {
        let (rc, body, events) = wire_call_at(
            state,
            "interact-gui",
            VERB_PRESENTER_REGISTER,
            json!({}),
            at_ms,
        );
        assert_eq!(rc, 0, "{body}");
        (
            serde_json::from_value(body["lease"].clone()).unwrap(),
            events,
        )
    }

    fn next_presentation(
        state: &mut InteractState,
        lease: &DialogPresenterLeaseV1,
        at_ms: u64,
    ) -> (Value, Vec<PropsEvent>) {
        let (rc, body, events) = wire_call_at(
            state,
            "interact-gui",
            VERB_PRESENTER_NEXT,
            json!({ "lease": lease }),
            at_ms,
        );
        assert_eq!(rc, 0, "{body}");
        (body["presentation"].clone(), events)
    }

    #[test]
    fn canonical_registered_local_caller_is_admitted() {
        let caller = authorize_caller(&command("musicd", None, None)).unwrap();
        assert_eq!(caller.origin, "musicd");
        assert_eq!(caller.owner, OwnerId::local("musicd"));

        let mut req = NotifyRequest::new("Unverified alert");
        req.urgency = Urgency::Critical;
        let mut state = InteractState::new();
        let (rc, body, _) = state.notify(
            req,
            caller.broker_caller(),
            NotifyOwnership::new(
                caller.owner.clone(),
                None,
                OwnerToken("o00000000000000000000000000000000".into()),
            ),
            1_000,
            NotifyHandle("n1".into()),
        );
        assert_eq!(rc, 0, "{body}");
        let record = &state.records()["n1"];
        assert_eq!(record.origin, "musicd");
        assert_eq!(record.urgency_override, None);
    }

    #[test]
    fn whitespace_padded_body_is_bounded_by_raw_bytes_before_json_normalisation() {
        let mut cmd = command("musicd", None, None);
        let compact = r#"{"summary":"small"}"#;
        cmd.body = format!(
            "{compact}{}",
            " ".repeat(MAX_REQUEST_BYTES + 1 - compact.len())
        );
        cmd.args = json!({"summary": "small"});

        let (rc, body) = resolve_bounded_args(&cmd).unwrap_err();
        assert_eq!(rc, 10);
        assert_eq!(
            serde_json::from_str::<Value>(&body).unwrap()["error"]["id"],
            cosmix_interaction_schema::VALIDATION_REQUEST_TOO_LARGE
        );
    }

    #[test]
    fn invalid_args_header_fallback_counts_only_consumed_body() {
        let mut cmd = command("musicd", None, None);
        let compact = r#"{"summary":"small"}"#;
        cmd.body = format!("{compact}{}", " ".repeat(MAX_REQUEST_BYTES - compact.len()));
        cmd.args = json!({"summary": "small"});
        cmd.headers.insert("args".into(), "{".into());

        let args = resolve_bounded_args(&cmd).unwrap().unwrap();
        assert_eq!(args["summary"], "small");
    }

    #[test]
    fn valid_args_header_adds_its_raw_bytes_to_body() {
        let mut cmd = command("musicd", None, None);
        cmd.body = " ".repeat(MAX_REQUEST_BYTES);
        cmd.headers
            .insert("args".into(), r#"{"summary":"small"}"#.into());

        let (_, body) = resolve_bounded_args(&cmd).unwrap_err();
        assert_eq!(
            serde_json::from_str::<Value>(&body).unwrap()["error"]["id"],
            cosmix_interaction_schema::VALIDATION_REQUEST_TOO_LARGE
        );
    }

    #[test]
    fn empty_or_noncanonical_caller_is_rejected() {
        for from in ["", "   ", "x", "mesh.peer", "BadName", "bad_name"] {
            let denied = authorize_caller(&command(from, None, None)).unwrap_err();
            assert_eq!(denied.id, "unregistered_caller", "from={from:?}");
            assert!(denied.message.contains("canonically registered local"));
            let (rc, body) = app_error(denied.id, &denied.message);
            assert_eq!(rc, 10);
            assert_eq!(
                serde_json::from_str::<Value>(&body).unwrap()["error"]["id"],
                "unregistered_caller"
            );
        }
    }

    #[test]
    fn interact_verbs_from_mesh_or_unstamped_origin_are_rejected() {
        // A canonically-named caller (would pass the service-name gate) whose
        // delivery is stamped `mesh` or arrives unstamped must be rejected on
        // locality alone — this is the presenter-spoof vector (a mesh peer
        // presenting `from="interact-gui"`). Cover every interact.* verb prefix.
        for verb in [
            "interact.notify",
            "interact.update",
            "interact.dialog-open",
            "interact.presenter-register",
        ] {
            for origin in [Some("mesh"), None] {
                let mut cmd = command("interact-gui", None, None);
                cmd.command = verb.to_string();
                match origin {
                    Some(value) => {
                        cmd.headers
                            .insert(BROKER_ORIGIN_HEADER.into(), value.into());
                    }
                    None => {
                        cmd.headers.remove(BROKER_ORIGIN_HEADER);
                    }
                }
                let denied = authorize_caller(&cmd).unwrap_err();
                assert_eq!(
                    denied.id, "remote_ingress_closed",
                    "verb={verb} origin={origin:?}"
                );
                assert!(denied.message.contains("node-local origin"));
            }
        }
    }

    #[test]
    fn non_interact_commands_bypass_the_locality_gate() {
        // noded-internal topic lifecycle notices arrive unstamped and are not
        // interact.* verbs; the locality gate must not touch them (they are
        // handled/ignored downstream). Verify an unstamped `topic.active` is
        // not rejected by `authorize_caller` for a locality reason.
        let mut cmd = command("interactd", None, None);
        cmd.command = "topic.active".to_string();
        cmd.headers.remove(BROKER_ORIGIN_HEADER);
        // `interactd` is a canonical service name, so this passes both gates.
        assert!(authorize_caller(&cmd).is_ok());
    }

    /// Run-time manifest directory rather than the `env!`-baked one: cargo
    /// exports `CARGO_MANIFEST_DIR` into the test process, and that names
    /// the tree cargo is actually running in, whereas `env!` records
    /// whichever tree last *compiled* the binary. The two diverge when one
    /// `CARGO_TARGET_DIR` is shared across several git worktrees of this
    /// repo. Falls back to the compile-time value when run outside cargo.
    fn manifest_dir() -> std::path::PathBuf {
        std::env::var_os("CARGO_MANIFEST_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")))
    }

    #[test]
    fn spec10_identity_matches_checked_in_sysusers_fragment() {
        let conf = std::fs::read_to_string(manifest_dir().join("../../_etc/sysusers/cosmix.conf"))
            .unwrap();
        let version = conf
            .lines()
            .find_map(|line| line.split("cosmix-daemon-identity v").nth(1))
            .and_then(|tail| tail.strip_suffix('.'))
            .expect("sysusers header must declare the SPEC-10 version");
        assert_eq!(version, "1.4.4");

        let row = conf
            .lines()
            .find(|line| {
                let fields: Vec<_> = line.split_whitespace().collect();
                fields.first() == Some(&"u") && fields.get(1) == Some(&"cosmix-interactd")
            })
            .expect("sysusers must carry the cosmix-interactd identity");
        assert_eq!(row.split_whitespace().nth(2), Some("517"));

        let mesh_group = conf
            .lines()
            .find(|line| {
                let fields: Vec<_> = line.split_whitespace().collect();
                fields.first() == Some(&"g") && fields.get(1) == Some(&"cosmix-mesh")
            })
            .expect("sysusers must retain the deployed cosmix-mesh group");
        assert_eq!(mesh_group.split_whitespace().nth(2), Some("516"));
    }

    #[test]
    fn wire_asserted_remote_identity_or_permissions_are_rejected() {
        for cmd in [
            command("musicd", Some("delta.example"), None),
            command("musicd", None, Some("[\"ctk.notify\"]")),
            command("musicd", Some("delta.example"), Some("[\"ctk.notify\"]")),
            {
                let mut cmd = command("musicd", None, None);
                cmd.headers
                    .insert("Signed_Ident".into(), "mesh:delta.example".into());
                cmd
            },
        ] {
            let denied = authorize_caller(&cmd).unwrap_err();
            assert_eq!(denied.id, "remote_identity_unavailable");
            assert!(denied.message.contains(CTK_NOTIFY));
            let (rc, body) = app_error(denied.id, &denied.message);
            assert_eq!(rc, 10);
            assert_eq!(
                serde_json::from_str::<Value>(&body).unwrap()["error"]["id"],
                "remote_identity_unavailable"
            );
        }
    }

    #[test]
    fn mock_presenter_happy_path_returns_value_and_lifecycle_edges() {
        let mut state = InteractState::with_dialog_instance_epoch(NonZeroU64::new(101).unwrap());
        let (handle, owner_token, mut events) = open_dialog(
            &mut state,
            "musicd",
            message_dialog("Question", None),
            1_000,
        );
        let (lease, registered) = register_presenter(&mut state, 1_001);
        events.extend(registered);
        let (presentation, next_events) = next_presentation(&mut state, &lease, 1_002);
        events.extend(next_events);
        let attempt_token = presentation["attempt_token"].as_u64().unwrap();
        assert_eq!(presentation["handle"], handle);

        let (rc, _, marked) = wire_call_at(
            &mut state,
            "interact-gui",
            VERB_PRESENTER_MARK_PRESENTED,
            json!({
                "lease": lease,
                "handle": handle,
                "attempt_token": attempt_token
            }),
            1_003,
        );
        assert_eq!(rc, 0);
        events.extend(marked);
        let (rc, _, resolved) = wire_call_at(
            &mut state,
            "interact-gui",
            VERB_PRESENTER_RESOLVE,
            json!({
                "lease": lease,
                "handle": handle,
                "attempt_token": attempt_token,
                "value": DialogValueV1::Message {}
            }),
            1_004,
        );
        assert_eq!(rc, 0);
        events.extend(resolved);

        let (rc, result, result_events) = wire_call_at(
            &mut state,
            "musicd",
            VERB_DIALOG_RESULT,
            json!({"handle": handle, "owner_token": owner_token}),
            1_005,
        );
        assert_eq!(rc, 0, "{result}");
        assert_eq!(result["state"], "resolved");
        assert_eq!(result["value"]["kind"], "message");
        assert!(result_events.is_empty());
        assert!(events.iter().any(|event| matches!(
            event,
            PropsEvent::Dialog(DialogPropsTransition {
                new: Some(DialogStateV1::Resolved),
                ..
            })
        )));
    }

    #[test]
    fn presenter_auth_matrix_rejects_anonymous_remote_and_wrong_service() {
        let presenter_verbs = [
            VERB_PRESENTER_REGISTER,
            VERB_PRESENTER_RELEASE,
            VERB_PRESENTER_NEXT,
            VERB_PRESENTER_MARK_PRESENTED,
            VERB_PRESENTER_RESOLVE,
            VERB_PRESENTER_FAIL,
            VERB_PRESENTER_PROGRESS_CANCEL,
        ];
        let mut state = InteractState::with_dialog_instance_epoch(NonZeroU64::new(102).unwrap());

        for verb in presenter_verbs {
            let (rc, body, _) = wire_call_at(&mut state, "", verb, Value::Null, 1_000);
            assert_eq!(rc, 10, "{verb}");
            assert_eq!(body["error"]["id"], "unregistered_caller", "{verb}");

            let (rc, body, _) = wire_call_at(&mut state, "musicd", verb, Value::Null, 1_000);
            assert_eq!(rc, 10, "{verb}");
            assert_eq!(body["error"]["id"], "presenter_forbidden", "{verb}");
        }

        for header in ["source_peer", "permissions", "signed_ident"] {
            let mut cmd = command("interact-gui", None, None);
            cmd.command = VERB_PRESENTER_REGISTER.into();
            cmd.args = json!({});
            cmd.headers.insert(header.into(), "wire-asserted".into());
            let error = authorize_caller(&cmd).unwrap_err();
            assert_eq!(error.id, "remote_identity_unavailable", "{header}");
        }
    }

    #[test]
    fn old_wire_lease_is_fenced_after_interactd_restart() {
        let mut before = InteractState::with_dialog_instance_epoch(NonZeroU64::new(201).unwrap());
        let (old_lease, _) = register_presenter(&mut before, 1_000);
        let mut after = InteractState::with_dialog_instance_epoch(NonZeroU64::new(202).unwrap());
        let (fresh_lease, _) = register_presenter(&mut after, 1_000);
        assert_eq!(old_lease.generation, fresh_lease.generation);
        assert_ne!(old_lease.instance_epoch, fresh_lease.instance_epoch);

        let (rc, body, _) = wire_call_at(
            &mut after,
            "interact-gui",
            VERB_PRESENTER_NEXT,
            json!({"lease": old_lease}),
            1_001,
        );
        assert_eq!(rc, 10);
        assert_eq!(body["error"]["id"], "stale_lease");
    }

    #[test]
    fn presenter_crash_requeues_with_fresh_attempt_and_stale_echo_is_rejected() {
        let mut state = InteractState::with_dialog_instance_epoch(NonZeroU64::new(103).unwrap());
        let (handle, owner_token, _) =
            open_dialog(&mut state, "musicd", message_dialog("Crash", None), 1_000);
        let (lease_a, _) = register_presenter(&mut state, 1_001);
        let (first, _) = next_presentation(&mut state, &lease_a, 1_002);
        let token_a = first["attempt_token"].as_u64().unwrap();

        let (lease_b, _) = register_presenter(&mut state, 1_003);
        let (rc, stale_lease, _) = wire_call_at(
            &mut state,
            "interact-gui",
            VERB_PRESENTER_RESOLVE,
            json!({
                "lease": lease_a,
                "handle": handle,
                "attempt_token": token_a,
                "value": DialogValueV1::Message {}
            }),
            1_004,
        );
        assert_eq!(rc, 10);
        assert_eq!(stale_lease["error"]["id"], "stale_lease");

        let (second, _) = next_presentation(&mut state, &lease_b, 1_005);
        let token_b = second["attempt_token"].as_u64().unwrap();
        assert_ne!(token_a, token_b);
        let (rc, stale_attempt, _) = wire_call_at(
            &mut state,
            "interact-gui",
            VERB_PRESENTER_RESOLVE,
            json!({
                "lease": lease_b,
                "handle": handle,
                "attempt_token": token_a,
                "value": DialogValueV1::Message {}
            }),
            1_006,
        );
        assert_eq!(rc, 10);
        assert_eq!(stale_attempt["error"]["id"], "stale_attempt");

        let (rc, _, _) = wire_call_at(
            &mut state,
            "interact-gui",
            VERB_PRESENTER_MARK_PRESENTED,
            json!({"lease": lease_b, "handle": handle, "attempt_token": token_b}),
            1_007,
        );
        assert_eq!(rc, 0);
        let (rc, _, _) = wire_call_at(
            &mut state,
            "interact-gui",
            VERB_PRESENTER_RESOLVE,
            json!({
                "lease": lease_b,
                "handle": handle,
                "attempt_token": token_b,
                "value": DialogValueV1::Message {}
            }),
            1_008,
        );
        assert_eq!(rc, 0);
        let (rc, result, _) = wire_call_at(
            &mut state,
            "musicd",
            VERB_DIALOG_RESULT,
            json!({"handle": handle, "owner_token": owner_token}),
            1_009,
        );
        assert_eq!(rc, 0);
        assert_eq!(result["state"], "resolved");
    }

    #[test]
    fn repeated_pre_display_presenter_replacement_quarantines_dialog() {
        let mut state = InteractState::with_dialog_instance_epoch(NonZeroU64::new(104).unwrap());
        let (handle, owner_token, _) =
            open_dialog(&mut state, "musicd", message_dialog("Poison", None), 1_000);
        let (mut lease, _) = register_presenter(&mut state, 1_001);
        let _ = next_presentation(&mut state, &lease, 1_002);

        for retry in 0..=MAX_DIALOG_PRE_DISPLAY_REQUEUES {
            (lease, _) = register_presenter(&mut state, 2_000 + u64::from(retry));
            let (presentation, _) = next_presentation(&mut state, &lease, 2_100 + u64::from(retry));
            if retry < MAX_DIALOG_PRE_DISPLAY_REQUEUES {
                assert_eq!(presentation["handle"], handle);
            } else {
                assert!(presentation.is_null());
            }
        }

        let (rc, result, _) = wire_call_at(
            &mut state,
            "musicd",
            VERB_DIALOG_RESULT,
            json!({"handle": handle, "owner_token": owner_token}),
            3_000,
        );
        assert_eq!(rc, 0);
        assert_eq!(result["state"], "failed");
        assert!(result.get("value").is_none());
    }

    #[test]
    fn owner_self_resolve_is_blocked_by_identity_gate_and_broker_owner_rule() {
        let mut state = InteractState::with_dialog_instance_epoch(NonZeroU64::new(105).unwrap());
        let (foreign_handle, _, _) =
            open_dialog(&mut state, "musicd", message_dialog("Gate", None), 1_000);
        let (rc, gated, _) = wire_call_at(
            &mut state,
            "musicd",
            VERB_PRESENTER_RESOLVE,
            json!({
                "lease": {
                    "presenter_service": "musicd",
                    "generation": 1,
                    "instance_epoch": 105
                },
                "handle": foreign_handle,
                "attempt_token": 1,
                "value": DialogValueV1::Message {}
            }),
            1_001,
        );
        assert_eq!(rc, 10);
        assert_eq!(gated["error"]["id"], "presenter_forbidden");

        let mut state = InteractState::with_dialog_instance_epoch(NonZeroU64::new(109).unwrap());
        let (owned_handle, _, _) = open_dialog(
            &mut state,
            "interact-gui",
            message_dialog("Owner", None),
            1_002,
        );
        let (lease, _) = register_presenter(&mut state, 1_003);
        let (presentation, _) = next_presentation(&mut state, &lease, 1_004);
        let token = presentation["attempt_token"].as_u64().unwrap();
        let (rc, _, _) = wire_call_at(
            &mut state,
            "interact-gui",
            VERB_PRESENTER_MARK_PRESENTED,
            json!({"lease": lease, "handle": owned_handle, "attempt_token": token}),
            1_005,
        );
        assert_eq!(rc, 0);
        let (rc, denied, _) = wire_call_at(
            &mut state,
            "interact-gui",
            VERB_PRESENTER_RESOLVE,
            json!({
                "lease": lease,
                "handle": owned_handle,
                "attempt_token": token,
                "value": DialogValueV1::Message {}
            }),
            1_006,
        );
        assert_eq!(rc, 10);
        assert_eq!(denied["error"]["id"], "owner_cannot_resolve");
    }

    #[test]
    fn deadline_arms_one_shot_and_maintenance_expires_idle_dialog() {
        let mut state = InteractState::with_dialog_instance_epoch(NonZeroU64::new(106).unwrap());
        let opened_at = 5_000;
        let (handle, owner_token, _) = open_dialog(
            &mut state,
            "musicd",
            message_dialog("Deadline", Some(MIN_DIALOG_DEADLINE_MS)),
            opened_at,
        );
        let expected = opened_at + MIN_DIALOG_DEADLINE_MS;
        assert_eq!(state.dialog_next_maintenance_at_ms(), Some(expected));
        let (schedule_tx, schedule_rx) = watch::channel(None);
        reschedule_dialog_maintenance(&state, &schedule_tx);
        assert_eq!(*schedule_rx.borrow(), Some(expected));

        state.dialog_maintain(expected);
        let events = drain_props_events(&mut state);
        assert!(events.iter().any(|event| matches!(
            event,
            PropsEvent::Dialog(DialogPropsTransition {
                new: Some(DialogStateV1::Expired),
                ..
            })
        )));
        let (rc, result, _) = wire_call_at(
            &mut state,
            "musicd",
            VERB_DIALOG_RESULT,
            json!({"handle": handle, "owner_token": owner_token}),
            expected,
        );
        assert_eq!(rc, 0);
        assert_eq!(result["state"], "expired");
        assert!(result.get("value").is_none());
    }

    #[test]
    fn progress_update_presentation_cancel_and_completion_use_current_snapshot() {
        let mut state = InteractState::with_dialog_instance_epoch(NonZeroU64::new(107).unwrap());
        let (handle, owner_token, _) =
            open_dialog(&mut state, "musicd", progress_dialog("Starting"), 1_000);
        let (rc, _, progress_events) = wire_call_at(
            &mut state,
            "musicd",
            VERB_DIALOG_PROGRESS_UPDATE,
            json!({
                "handle": handle,
                "owner_token": owner_token,
                "patch": {
                    "message": "Uploading",
                    "progress": {"mode": "determinate", "current": 5, "total": 10}
                }
            }),
            1_001,
        );
        assert_eq!(rc, 0);
        let progress_path = format!("dialogs.{handle}.progress_fraction");
        assert!(progress_events.iter().any(|event| {
            transition_message(event).get("path") == Some(progress_path.as_str())
        }));
        assert_eq!(state.dialog_records()[&handle].progress_fraction, Some(0.5));

        let (lease, _) = register_presenter(&mut state, 1_002);
        let (presentation, _) = next_presentation(&mut state, &lease, 1_003);
        assert_eq!(presentation["progress"]["message"], "Uploading");
        assert_eq!(presentation["progress"]["progress"]["current"], 5);
        let token = presentation["attempt_token"].as_u64().unwrap();
        let (rc, _, _) = wire_call_at(
            &mut state,
            "interact-gui",
            VERB_PRESENTER_MARK_PRESENTED,
            json!({"lease": lease, "handle": handle, "attempt_token": token}),
            1_004,
        );
        assert_eq!(rc, 0);
        let (rc, _, _) = wire_call_at(
            &mut state,
            "interact-gui",
            VERB_PRESENTER_PROGRESS_CANCEL,
            json!({"lease": lease, "handle": handle, "attempt_token": token}),
            1_005,
        );
        assert_eq!(rc, 0);
        let (rc, result, _) = wire_call_at(
            &mut state,
            "musicd",
            VERB_DIALOG_RESULT,
            json!({"handle": handle, "owner_token": owner_token}),
            1_006,
        );
        assert_eq!(rc, 0);
        assert_eq!(result["state"], "cancel-requested");

        let (rc, _, _) = wire_call_at(
            &mut state,
            "musicd",
            VERB_DIALOG_PROGRESS_COMPLETE,
            json!({
                "handle": handle,
                "owner_token": owner_token,
                "completion": {"outcome": "cancelled"}
            }),
            1_007,
        );
        assert_eq!(rc, 0);
        let (rc, result, _) = wire_call_at(
            &mut state,
            "musicd",
            VERB_DIALOG_RESULT,
            json!({"handle": handle, "owner_token": owner_token}),
            1_008,
        );
        assert_eq!(rc, 0);
        assert_eq!(result["state"], "resolved");
        assert_eq!(result["value"]["kind"], "progress");
        assert_eq!(result["value"]["completion"]["outcome"], "cancelled");
    }

    #[test]
    fn dialog_raw_cap_rejects_body_before_typed_serde() {
        let mut state = InteractState::with_dialog_instance_epoch(NonZeroU64::new(108).unwrap());
        let mut cmd = command("musicd", None, None);
        cmd.command = VERB_DIALOG_OPEN.into();
        cmd.body = " ".repeat(DIALOG_RAW_INGRESS_CAP + 1);
        cmd.args = message_dialog("small parsed value", None);
        let caller = authorize_caller(&cmd).unwrap();
        let (delivery_tx, _delivery_rx) = mpsc::channel(1);
        let budget = DeliveryByteBudget::new(MAX_DELIVERY_QUEUE_BYTES);

        let (rc, body) = dispatch_at(&mut state, &cmd, &caller, &delivery_tx, &budget, 1_000);
        assert_eq!(rc, 10);
        let body: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            body["error"]["id"],
            cosmix_interaction_schema::VALIDATION_REQUEST_TOO_LARGE
        );
        assert!(state.dialog_records().is_empty());
    }

    #[test]
    fn mutation_without_owner_token_has_stable_bus_error() {
        let caller = authorize_caller(&command("musicd", None, None)).unwrap();
        let mut state = InteractState::new();
        let (delivery_tx, _delivery_rx) = mpsc::channel(4);

        for verb in ["interact.update", "interact.dismiss"] {
            let mut cmd = command("musicd", None, None);
            cmd.command = verb.into();
            cmd.args = json!({ "handle": "n1", "summary": "changed" });
            let budget = DeliveryByteBudget::new(MAX_DELIVERY_QUEUE_BYTES);
            let (rc, body) = dispatch(&mut state, &cmd, &caller, &delivery_tx, &budget);
            assert_eq!(rc, 10);
            assert_eq!(
                serde_json::from_str::<Value>(&body).unwrap()["error"]["id"],
                "owner_token_required"
            );
        }
    }

    #[test]
    fn props_watch_returns_the_connection_scoped_topic_contract() {
        let caller = authorize_caller(&command("musicd", None, None)).unwrap();
        let mut state = InteractState::new();
        let (_, _, job) = state.notify(
            NotifyRequest::new("watermark"),
            Caller::local("musicd"),
            NotifyOwnership::new(
                OwnerId::local("musicd"),
                None,
                OwnerToken("o00000000000000000000000000000000".into()),
            ),
            1_000,
            NotifyHandle("n1".into()),
        );
        assert!(job.is_some());
        let (delivery_tx, _delivery_rx) = mpsc::channel(1);
        let mut cmd = command("musicd", None, None);
        cmd.command = "interact.props.watch".into();

        let budget = DeliveryByteBudget::new(MAX_DELIVERY_QUEUE_BYTES);
        let (rc, body) = dispatch(&mut state, &cmd, &caller, &delivery_tx, &budget);
        assert_eq!(rc, 0);
        let body: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["topic"], PROPS_CHANGED_TOPIC);
        assert_eq!(body["queue_capacity"], PROPS_EVENT_QUEUE_CAP);
        assert_eq!(body["event_sequence"], "daemon_session_monotonic");
        assert_eq!(body["event_seq"], 1);
    }

    #[test]
    fn props_reconnect_queue_is_bounded_with_oldest_eviction() {
        let transition = |index| {
            PropsEvent::Notification(PropsStateTransition {
                seq: index as u64 + 1,
                handle: NotifyHandle(format!("n{index}")),
                old: None,
                new: NotifyState::Queued,
            })
        };
        let mut pending = PendingPropsEvents::new();
        for index in 0..PROPS_EVENT_QUEUE_CAP {
            assert!(pending.push(transition(index)).is_none());
        }
        let dropped = pending.push(transition(PROPS_EVENT_QUEUE_CAP)).unwrap();
        assert!(matches!(
            dropped,
            PropsEvent::Notification(PropsStateTransition { handle, .. })
                if handle.as_str() == "n0"
        ));
        assert_eq!(pending.queue.len(), PROPS_EVENT_QUEUE_CAP);
        assert!(matches!(
            pending.queue.front().unwrap(),
            PropsEvent::Notification(PropsStateTransition { handle, .. })
                if handle.as_str() == "n1"
        ));
    }

    #[test]
    fn full_props_ingress_records_every_discarded_transition() {
        let (ingress, _rx) = PropsEventIngress::channel(1);
        let transition = |seq| {
            PropsEvent::Notification(PropsStateTransition {
                seq,
                handle: NotifyHandle(format!("n{seq}")),
                old: None,
                new: NotifyState::Queued,
            })
        };
        ingress.enqueue(vec![transition(1)]);
        ingress.enqueue(vec![transition(2), transition(3)]);
        assert_eq!(
            *ingress.loss.lock().expect("props loss tracker poisoned"),
            PropsEventLoss {
                lost_count: 2,
                through_seq: 3,
            }
        );
    }

    #[test]
    fn detected_loss_drains_real_receiver_backlog_before_gap_publication() {
        let transition = |seq| {
            PropsEvent::Notification(PropsStateTransition {
                seq,
                handle: NotifyHandle(format!("n{seq}")),
                old: None,
                new: NotifyState::Queued,
            })
        };
        let (ingress, mut rx) = PropsEventIngress::channel(3);
        let mut pending = PendingPropsEvents::new();
        pending.push(transition(1));
        ingress.enqueue(vec![transition(2)]);
        ingress.enqueue(vec![transition(3)]);
        ingress.enqueue(vec![transition(4)]);
        ingress.enqueue(vec![transition(5)]);

        let loss = take_loss_and_discard_backlog(&mut pending, &mut rx, &ingress.loss)
            .expect("full ingress records a gap");

        assert!(pending.queue.is_empty());
        assert!(matches!(
            rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        assert_eq!(loss.lost_count, 5);
        assert_eq!(loss.through_seq, 5);

        ingress.enqueue(vec![transition(6)]);
        assert_eq!(rx.try_recv().unwrap()[0].seq(), 6);
    }

    #[test]
    fn icon_policy_rejects_emoji_and_unknown_lucide_keys() {
        assert!(validate_icon_policy(None).is_ok());
        assert!(validate_icon_policy(Some(&IconRef::Lucide("music".into()))).is_ok());
        assert!(
            validate_icon_policy(Some(&IconRef::Emoji("party".into())))
                .unwrap_err()
                .contains("unsupported")
        );
        assert!(
            validate_icon_policy(Some(&IconRef::Lucide("made-up".into())))
                .unwrap_err()
                .contains("supported catalogue")
        );
    }

    #[test]
    fn update_rejects_unsupported_icon_before_queue_or_state_mutation() {
        let caller = authorize_caller(&command("musicd", None, None)).unwrap();
        let mut state = InteractState::new();
        let (delivery_tx, _delivery_rx) = mpsc::channel(1);
        delivery_tx
            .try_send(DeliveryCommand::Close {
                handle: NotifyHandle("occupied".into()),
                _bytes: test_byte_permit(),
            })
            .unwrap();
        let mut cmd = command("musicd", None, None);
        cmd.command = "interact.update".into();
        cmd.args = json!({
            "handle": "n1",
            "owner_token": "o00000000000000000000000000000000",
            "summary": "changed",
            "icon": { "emoji": "party" }
        });

        let budget = DeliveryByteBudget::new(MAX_DELIVERY_QUEUE_BYTES);
        let (rc, body) = dispatch(&mut state, &cmd, &caller, &delivery_tx, &budget);
        assert_eq!(rc, 10);
        assert_eq!(
            serde_json::from_str::<Value>(&body).unwrap()["error"]["id"],
            "unsupported"
        );
        assert!(state.records().is_empty());
    }

    #[test]
    fn reconnect_action_queue_is_bounded_fifo_with_oldest_eviction() {
        fn order(key: &str) -> DispatchOrder {
            DispatchOrder {
                service: "filemgr".into(),
                verb: "app.open".into(),
                handle: format!("n{key}"),
                key: key.into(),
                requested_by: "musicd".into(),
            }
        }

        let mut pending = PendingActions::new(2);
        assert!(pending.push(order("one")).is_none());
        assert!(pending.push(order("two")).is_none());
        assert_eq!(pending.push(order("three")).unwrap().key, "one");
        assert_eq!(pending.queue.pop_front().unwrap().key, "two");
        assert_eq!(pending.queue.pop_front().unwrap().key, "three");
    }

    #[test]
    fn action_dispatch_body_carries_broker_stamped_attribution() {
        let order = DispatchOrder {
            service: "filemgr".into(),
            verb: "app.open".into(),
            handle: "n1".into(),
            key: "open".into(),
            requested_by: "musicd".into(),
        };
        assert_eq!(
            action_args(&order),
            json!({
                "handle": "n1",
                "key": "open",
                "requested_by": "musicd",
            })
        );
    }

    #[test]
    fn click_dispatch_requires_target_to_remain_registered() {
        let order = DispatchOrder {
            service: "filemgr".into(),
            verb: "app.open".into(),
            handle: "n1".into(),
            key: "open".into(),
            requested_by: "musicd".into(),
        };
        assert!(action_target_is_registered(
            &order,
            &["musicd".into(), "filemgr".into()]
        ));
        assert!(!action_target_is_registered(&order, &["musicd".into()]));
    }

    #[test]
    fn delivery_byte_budget_rejects_aggregate_overflow_and_releases_on_drop() {
        let budget = DeliveryByteBudget::new(10);
        let first = budget.try_reserve(6).unwrap();
        assert!(budget.try_reserve(5).is_none());
        drop(first);
        assert!(budget.try_reserve(10).is_some());
    }

    #[tokio::test]
    async fn ambiguous_send_result_drops_attempted_action_without_retry() {
        let mut pending = PendingActions::new(2);
        for key in ["first", "second"] {
            pending.push(DispatchOrder {
                service: "filemgr".into(),
                verb: "app.open".into(),
                handle: "n1".into(),
                key: key.into(),
                requested_by: "musicd".into(),
            });
        }
        let attempted = Arc::new(Mutex::new(Vec::new()));
        let seen = attempted.clone();
        flush_actions_with(&mut pending, move |order| {
            seen.lock().unwrap().push(order.key);
            async { false }
        })
        .await;
        assert_eq!(*attempted.lock().unwrap(), ["first"]);
        assert_eq!(pending.queue.front().unwrap().key, "second");
    }

    struct GatedSink {
        started: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }

    #[async_trait]
    impl NotifySink for GatedSink {
        async fn show(
            &mut self,
            _handle: &NotifyHandle,
            _revision: u64,
            _view: &sink::NotifyView,
            _replaces: Option<&NotifyHandle>,
        ) -> Result<ShowOutcome, sink::SinkError> {
            self.started.notify_one();
            self.release.notified().await;
            Ok(ShowOutcome::Shown(None))
        }

        async fn close(&mut self, _handle: &NotifyHandle) -> Result<(), sink::SinkError> {
            Ok(())
        }

        fn retire(&mut self, _handle: &NotifyHandle) {}
    }

    struct FailingSink {
        closed: Arc<std::sync::atomic::AtomicUsize>,
        retired: Arc<std::sync::atomic::AtomicUsize>,
    }

    struct AmbiguousSink {
        started: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
        tombstone_resolved: Arc<std::sync::atomic::AtomicUsize>,
        closed: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait]
    impl NotifySink for AmbiguousSink {
        async fn show(
            &mut self,
            _handle: &NotifyHandle,
            _revision: u64,
            _view: &sink::NotifyView,
            _replaces: Option<&NotifyHandle>,
        ) -> Result<ShowOutcome, sink::SinkError> {
            self.started.notify_one();
            let release = self.release.clone();
            let resolved = self.tombstone_resolved.clone();
            Ok(ShowOutcome::Ambiguous(sink::ShowTombstone::new(
                async move {
                    release.notified().await;
                    resolved.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    sink::TombstoneResolution::NoHandle
                },
            )))
        }

        async fn close(&mut self, _handle: &NotifyHandle) -> Result<(), sink::SinkError> {
            self.closed
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }

        fn retire(&mut self, _handle: &NotifyHandle) {}
    }

    #[async_trait]
    impl NotifySink for FailingSink {
        async fn show(
            &mut self,
            _handle: &NotifyHandle,
            _revision: u64,
            _view: &sink::NotifyView,
            _replaces: Option<&NotifyHandle>,
        ) -> Result<ShowOutcome, sink::SinkError> {
            Err(sink::SinkError::Backend("test failure".into()))
        }

        async fn close(&mut self, _handle: &NotifyHandle) -> Result<(), sink::SinkError> {
            self.closed
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }

        fn retire(&mut self, _handle: &NotifyHandle) {
            self.retired
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn blocked_sink_does_not_hold_state_or_delay_queued_acknowledgement() {
        let state = Arc::new(Mutex::new(InteractState::new()));
        let (body, job) = {
            let mut state = state.lock().unwrap();
            let (rc, body, job) = state.notify(
                NotifyRequest::new("slow desktop"),
                Caller::local("musicd"),
                NotifyOwnership::new(
                    OwnerId::local("musicd"),
                    None,
                    OwnerToken("o00000000000000000000000000000000".into()),
                ),
                1_000,
                NotifyHandle("n1".into()),
            );
            assert_eq!(rc, 0);
            (body, job.unwrap())
        };
        assert_eq!(
            serde_json::from_str::<Value>(&body).unwrap()["handle"],
            "n1"
        );

        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let (delivery_tx, delivery_rx) = mpsc::channel(1);
        let (resolved_tx, _resolved_rx) = mpsc::channel(1);
        let (props_events, _props_rx) = PropsEventIngress::channel(1);
        let worker = tokio::spawn(run_delivery(
            delivery_rx,
            Box::new(GatedSink {
                started: started.clone(),
                release: release.clone(),
            }),
            state.clone(),
            resolved_tx,
            props_events,
        ));
        delivery_tx
            .send(DeliveryCommand::Show {
                job: Box::new(job),
                registry_client: None,
                _bytes: test_byte_permit(),
            })
            .await
            .unwrap();
        started.notified().await;

        assert_eq!(
            state.lock().unwrap().records()["n1"].state,
            NotifyState::Queued
        );
        release.notify_one();
        drop(delivery_tx);
        worker.await.unwrap();
        assert_eq!(
            state.lock().unwrap().records()["n1"].state,
            NotifyState::Shown
        );
    }

    #[tokio::test]
    async fn failed_delivery_marks_failed_and_releases_sink_state() {
        let state = Arc::new(Mutex::new(InteractState::new()));
        let job = state
            .lock()
            .unwrap()
            .notify(
                NotifyRequest::new("will fail"),
                Caller::local("musicd"),
                NotifyOwnership::new(
                    OwnerId::local("musicd"),
                    None,
                    OwnerToken("o00000000000000000000000000000000".into()),
                ),
                1_000,
                NotifyHandle("n1".into()),
            )
            .2
            .unwrap();
        let closed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let retired = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (delivery_tx, delivery_rx) = mpsc::channel(1);
        let (resolved_tx, _resolved_rx) = mpsc::channel(1);
        let (props_events, _props_rx) = PropsEventIngress::channel(1);
        let worker = tokio::spawn(run_delivery(
            delivery_rx,
            Box::new(FailingSink {
                closed: closed.clone(),
                retired: retired.clone(),
            }),
            state.clone(),
            resolved_tx,
            props_events,
        ));
        delivery_tx
            .send(DeliveryCommand::Show {
                job: Box::new(job),
                registry_client: None,
                _bytes: test_byte_permit(),
            })
            .await
            .unwrap();
        drop(delivery_tx);
        worker.await.unwrap();

        assert_eq!(
            state.lock().unwrap().records()["n1"].state,
            NotifyState::Failed
        );
        assert_eq!(closed.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(retired.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn ambiguous_show_stays_queued_until_tombstone_resolves() {
        let state = Arc::new(Mutex::new(InteractState::new()));
        let mut request = NotifyRequest::new("ambiguous");
        request.dedupe_key = Some("job".into());
        let job = state
            .lock()
            .unwrap()
            .notify(
                request.clone(),
                Caller::local("musicd"),
                NotifyOwnership::new(
                    OwnerId::local("musicd"),
                    None,
                    OwnerToken("o00000000000000000000000000000000".into()),
                ),
                1_000,
                NotifyHandle("n1".into()),
            )
            .2
            .unwrap();
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let tombstone_resolved = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let closed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (delivery_tx, delivery_rx) = mpsc::channel(1);
        let (resolved_tx, _resolved_rx) = mpsc::channel(1);
        let (props_events, _props_rx) = PropsEventIngress::channel(1);
        let worker = tokio::spawn(run_delivery(
            delivery_rx,
            Box::new(AmbiguousSink {
                started: started.clone(),
                release: release.clone(),
                tombstone_resolved: tombstone_resolved.clone(),
                closed: closed.clone(),
            }),
            state.clone(),
            resolved_tx,
            props_events,
        ));
        delivery_tx
            .send(DeliveryCommand::Show {
                job: Box::new(job),
                registry_client: None,
                _bytes: test_byte_permit(),
            })
            .await
            .unwrap();
        drop(delivery_tx);
        started.notified().await;
        tokio::task::yield_now().await;
        assert_eq!(
            state.lock().unwrap().records()["n1"].state,
            NotifyState::Queued
        );

        release.notify_one();
        worker.await.unwrap();
        assert_eq!(
            tombstone_resolved.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert_eq!(closed.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(
            state.lock().unwrap().records()["n1"].state,
            NotifyState::Failed
        );

        let (_, _, fresh) = state.lock().unwrap().notify(
            request,
            Caller::local("musicd"),
            NotifyOwnership::new(
                OwnerId::local("musicd"),
                None,
                OwnerToken("o11111111111111111111111111111111".into()),
            ),
            2_000,
            NotifyHandle("n2".into()),
        );
        assert_eq!(fresh.unwrap().handle, NotifyHandle("n2".into()));
    }
}
