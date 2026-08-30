//! The daemon's in-process interaction state — the memory-backed
//! `interactions` collection plus the verb handlers, wrapping the headless
//! [`cosmix_interaction_broker`] decision core.
//!
//! This is the testable seam: the Bus transport (`main.rs`) reduces each request
//! to `(NotifyRequest, Caller, now_ms, fresh_handle)` and calls one method here.
//! State returns an immutable [`DeliveryJob`] to the async worker; this module
//! has no clock, bus, or tokio dependency and never performs external I/O.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::num::NonZeroU64;

use cosmix_interaction_broker::{
    Caller, DialogBroker, DialogBrokerError, DialogSnapshot, DialogTransition,
    DialogTransitionBatch, DialogTransitionCause, NotifyBroker, NotifyDecision,
    PresentationAttemptToken, PresenterLease, RejectReason,
};
use cosmix_interaction_schema::{
    DialogOpenRequestV1, DialogOpenResponse, DialogPresentationV1, DialogPresenterLeaseV1,
    DialogPresenterNextResponseV1, DialogPresenterRegisterResponseV1, DialogProgressCompletionV1,
    DialogProgressPatchV1, DialogProgressSnapshotV1, DialogProgressValueV1, DialogResultResponseV1,
    DialogStateV1, DialogValueV1, Dispatch, InteractionHandle, NotifyHandle,
    NotifyMutationResponse, NotifyRecord, NotifyRequest, NotifyResponse, NotifyState, OwnerToken,
    Urgency, ValidationError,
};
use serde_json::json;
use subtle::ConstantTimeEq;

use crate::err;
use crate::sink::NotifyView;

/// Maximum terminal records retained in the props snapshot. Live/queued records
/// are never evicted; the oldest terminal transition is removed first.
pub const TERMINAL_RECORD_CAP: usize = 256;

/// One observable lifecycle transition destined for
/// `interact.props.changed`. `old = None` is the initial `Queued` insertion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PropsStateTransition {
    /// Monotonic for this daemon process. Consumers use it to reject duplicate
    /// retries and detect a missing lifecycle edge independently of noded's
    /// per-topic publish sequence.
    pub seq: u64,
    pub handle: NotifyHandle,
    pub old: Option<NotifyState>,
    pub new: NotifyState,
}

/// One dialog lifecycle edge projected through `interact.props.changed`.
#[derive(Debug, Clone, PartialEq)]
pub struct DialogPropsTransition {
    pub seq: u64,
    pub handle: InteractionHandle,
    pub old: Option<DialogStateV1>,
    /// `None` means the retained broker record was evicted.
    pub new: Option<DialogStateV1>,
    pub cause: DialogTransitionCause,
    pub old_progress_fraction: Option<f64>,
    pub new_progress_fraction: Option<f64>,
}

/// Content-free dialog record retained solely for the props snapshot.
#[derive(Debug, Clone, PartialEq)]
pub struct DialogPropsRecord {
    pub handle: InteractionHandle,
    pub origin: String,
    pub state: DialogStateV1,
    pub created_at_ms: u64,
    pub progress_fraction: Option<f64>,
}

/// The daemon-side presenter session. The wire lease is only an echoable
/// coordinate; only `internal_lease` can authorise broker calls.
struct PresenterSession {
    internal_lease: PresenterLease,
    wire_lease: DialogPresenterLeaseV1,
    attempts: HashMap<InteractionHandle, PresentationAttemptToken>,
}

/// One handle's current delivered generation. Events must match all three
/// identity fields before they can mutate state or dispatch an action.
struct Route {
    revision: u64,
    fd_id: u32,
    /// `action key` → dispatch target. Only actions with an `on_invoke` appear;
    /// a keyless action (dismiss-only) still marks the record but fires nothing.
    dispatch: HashMap<String, Dispatch>,
}

/// What the daemon should fire when an action is invoked: `send <service> <verb>
/// handle=<h> key=<k> requested_by=<service>` (notify.v1 §2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchOrder {
    pub service: String,
    pub verb: String,
    pub handle: String,
    pub key: String,
    /// Notify-time broker-stamped requesting service, never caller payload.
    pub requested_by: String,
}

/// Result of consuming one desktop action signal. The notification always
/// becomes terminal; `dispatch` is absent for a dismiss-only/default action.
#[derive(Debug)]
pub struct ActionOutcome {
    pub dispatch: Option<DispatchOrder>,
}

/// A terminal sink event after generation validation and route resolution.
#[derive(Debug)]
pub enum TerminalOutcome {
    Action(ActionOutcome),
    Closed,
}

/// Generation-bound signal received from the desktop listener.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliverySignal {
    Action { fd_id: u32, key: String },
    Closed { fd_id: u32, kind: ClosedKind },
}

/// Result of offering a desktop signal to the state handshake.
#[derive(Debug)]
pub enum SignalResult {
    /// The exact delivery generation is still queued; the event is retained
    /// until its route is installed by [`InteractState::complete_delivery`].
    Buffered,
    /// The signal belongs to an old/terminal/unknown generation.
    Stale,
    /// The signal matched the installed route and reached a terminal state.
    Resolved(TerminalOutcome),
}

/// Result of resolving a worker delivery attempt.
#[derive(Debug)]
pub struct CompletionResult {
    pub current: bool,
    pub terminal: Option<TerminalOutcome>,
}

impl CompletionResult {
    fn stale() -> Self {
        Self {
            current: false,
            terminal: None,
        }
    }

    fn applied(terminal: Option<TerminalOutcome>) -> Self {
        Self {
            current: true,
            terminal,
        }
    }
}

/// How the notification daemon reported a close, distilled to the two terminal
/// states notify.v1 records (freedesktop reason 1 = expired; 2/3/other = a
/// dismissal, whether by the user or our own `CloseNotification`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClosedKind {
    Expired,
    Dismissed,
}

/// The broker-reported local service name associated with a notification.
/// This is useful attribution, but it is not durable mutation authority: a
/// disconnected named service may later re-register. The per-notification
/// owner token remains mandatory. Same-name re-registration is the residual
/// gap until the control-plane authority work in B-2/B-3.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerId {
    service: String,
}

impl OwnerId {
    pub fn local(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HandleOwnership {
    caller: OwnerId,
    token: OwnerToken,
}

/// Ownership context for one notify attempt. A fresh delivery consumes the
/// server-minted token; a dedupe replacement must present the retained token.
pub struct NotifyOwnership {
    caller: OwnerId,
    supplied_token: Option<OwnerToken>,
    fresh_token: OwnerToken,
}

impl NotifyOwnership {
    pub fn new(
        caller: OwnerId,
        supplied_token: Option<OwnerToken>,
        fresh_token: OwnerToken,
    ) -> Self {
        Self {
            caller,
            supplied_token,
            fresh_token,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryKind {
    Notify,
    Update,
}

/// Immutable work handed to the asynchronous delivery worker after queue-time
/// policy and ownership checks have succeeded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryJob {
    pub handle: NotifyHandle,
    pub revision: u64,
    pub request: NotifyRequest,
    pub replaces: Option<NotifyHandle>,
    pub kind: DeliveryKind,
    origin: String,
    urgency: Urgency,
}

impl DeliveryJob {
    pub fn view(&self) -> NotifyView {
        NotifyView::render(&self.origin, self.urgency, &self.request)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryCompletion {
    Shown(Option<u32>),
    Failed,
}

/// The live interaction state: the broker's decision core, the stored
/// `interactions` collection (keyed by opaque handle), and pending revisions.
pub struct InteractState {
    broker: NotifyBroker,
    dialog_broker: DialogBroker,
    presenter: Option<PresenterSession>,
    /// All dialog handles known to this daemon session. This permits a full
    /// broker projection rebuild after bounded transition-buffer overflow even
    /// though the broker deliberately exposes no content-bearing record list.
    dialog_handles: HashSet<InteractionHandle>,
    dialog_records: BTreeMap<String, DialogPropsRecord>,
    dialog_props_transitions: Vec<DialogPropsTransition>,
    dialog_props_resync_needed: bool,
    /// Live + terminal notifications by handle string. `BTreeMap` gives a stable
    /// order for the props snapshot independent of insertion.
    records: BTreeMap<String, NotifyRecord>,
    /// Creating caller attribution plus an unguessable mutation capability by
    /// handle. Neither field is projected through the read-only props surface.
    owners: BTreeMap<String, HandleOwnership>,
    /// Current action/close route by opaque handle. The route carries both the
    /// delivery revision and freedesktop id, so neither old listeners nor id
    /// reuse can target a replacement generation.
    routes: HashMap<String, Route>,
    /// A terminal event may beat `show` completion and route installation. Keep
    /// at most one event for each queued generation until the worker handshakes.
    pending_signals: HashMap<(String, u64), DeliverySignal>,
    /// Current delivery revision per handle. Async completions from superseded
    /// jobs are ignored rather than rewriting newer state.
    revisions: BTreeMap<String, u64>,
    /// Terminal handles in transition order for bounded retention.
    terminal_order: VecDeque<String>,
    /// Transitions produced by the current synchronous state operation. The
    /// daemon drains this immediately into its bounded publisher channel; a
    /// vector is required because delivery completion can record `Shown` and a
    /// buffered terminal signal atomically in one operation.
    props_transitions: Vec<PropsStateTransition>,
    /// Next daemon-session sequence for the lifecycle event surface.
    next_props_seq: u64,
}

impl InteractState {
    pub fn new() -> Self {
        Self::with_dialog_instance_epoch(draw_dialog_instance_epoch())
    }

    pub(crate) fn with_dialog_instance_epoch(instance_epoch: NonZeroU64) -> Self {
        InteractState {
            broker: NotifyBroker::v1(),
            dialog_broker: DialogBroker::default().with_instance_epoch(instance_epoch),
            presenter: None,
            dialog_handles: HashSet::new(),
            dialog_records: BTreeMap::new(),
            dialog_props_transitions: Vec::new(),
            dialog_props_resync_needed: false,
            records: BTreeMap::new(),
            owners: BTreeMap::new(),
            routes: HashMap::new(),
            pending_signals: HashMap::new(),
            revisions: BTreeMap::new(),
            terminal_order: VecDeque::new(),
            props_transitions: Vec::new(),
            next_props_seq: 0,
        }
    }

    /// The stored `interactions` collection, for the read-only props surface.
    pub fn records(&self) -> &BTreeMap<String, NotifyRecord> {
        &self.records
    }

    pub fn dialog_records(&self) -> &BTreeMap<String, DialogPropsRecord> {
        &self.dialog_records
    }

    /// Current daemon-session lifecycle sequence. Props snapshots and watch
    /// discovery expose this watermark so a newly attached consumer can anchor
    /// continuity even if its first topic frame is dropped by noded fan-out.
    pub fn props_event_seq(&self) -> u64 {
        self.next_props_seq
    }

    /// Drain lifecycle transitions produced since the previous daemon call.
    pub fn take_props_transitions(&mut self) -> Vec<PropsStateTransition> {
        std::mem::take(&mut self.props_transitions)
    }

    pub fn take_dialog_props_transitions(&mut self) -> Vec<DialogPropsTransition> {
        std::mem::take(&mut self.dialog_props_transitions)
    }

    pub fn take_dialog_props_resync_needed(&mut self) -> bool {
        std::mem::take(&mut self.dialog_props_resync_needed)
    }

    pub fn dialog_open(
        &mut self,
        owner_service: &str,
        owner_token: OwnerToken,
        request: DialogOpenRequestV1,
        now_ms: u64,
        fresh_handle: InteractionHandle,
    ) -> Result<DialogOpenResponse, DialogBrokerError> {
        let result = self.dialog_broker.open(
            owner_service,
            owner_token,
            request,
            now_ms,
            fresh_handle.clone(),
        );
        if result.is_ok() {
            self.dialog_handles.insert(fresh_handle);
        }
        self.finish_dialog_mutation(now_ms);
        result
    }

    pub fn dialog_progress_update(
        &mut self,
        owner_service: &str,
        owner_token: &OwnerToken,
        handle: &InteractionHandle,
        patch: DialogProgressPatchV1,
        now_ms: u64,
    ) -> Result<(), DialogBrokerError> {
        let result =
            self.dialog_broker
                .update_progress(owner_service, owner_token, handle, patch, now_ms);
        self.finish_dialog_mutation(now_ms);
        result
    }

    pub fn dialog_progress_complete(
        &mut self,
        owner_service: &str,
        owner_token: &OwnerToken,
        handle: &InteractionHandle,
        completion: DialogProgressCompletionV1,
        now_ms: u64,
    ) -> Result<(), DialogBrokerError> {
        let result = self.dialog_broker.complete_progress(
            owner_service,
            owner_token,
            handle,
            completion,
            now_ms,
        );
        self.finish_dialog_mutation(now_ms);
        result
    }

    pub fn dialog_cancel(
        &mut self,
        owner_service: &str,
        owner_token: &OwnerToken,
        handle: &InteractionHandle,
        now_ms: u64,
    ) -> Result<(), DialogBrokerError> {
        let result = self
            .dialog_broker
            .cancel(owner_service, owner_token, handle, now_ms);
        self.finish_dialog_mutation(now_ms);
        result
    }

    pub fn dialog_result(
        &mut self,
        owner_service: &str,
        owner_token: &OwnerToken,
        handle: &InteractionHandle,
        now_ms: u64,
    ) -> Result<DialogResultResponseV1, DialogBrokerError> {
        // A delayed one-shot must not let a due deadline appear live on a read.
        self.dialog_maintain(now_ms);
        self.dialog_broker
            .result(owner_service, owner_token, handle)
    }

    pub fn presenter_register(
        &mut self,
        presenter_service: &str,
        now_ms: u64,
    ) -> Result<DialogPresenterRegisterResponseV1, DialogBrokerError> {
        let result = self
            .dialog_broker
            .register_presenter(presenter_service, now_ms);
        let response = result.map(|internal_lease| {
            let wire_lease = DialogPresenterLeaseV1 {
                presenter_service: internal_lease.service().to_string(),
                generation: internal_lease.generation(),
                instance_epoch: internal_lease.instance_epoch(),
            };
            self.presenter = Some(PresenterSession {
                internal_lease,
                wire_lease: wire_lease.clone(),
                attempts: HashMap::new(),
            });
            DialogPresenterRegisterResponseV1 { lease: wire_lease }
        });
        self.finish_dialog_mutation(now_ms);
        response
    }

    pub fn presenter_release(
        &mut self,
        wire_lease: &DialogPresenterLeaseV1,
        now_ms: u64,
    ) -> Result<(), DialogBrokerError> {
        let internal_lease = self.presenter_lease(wire_lease)?;
        let result = self
            .dialog_broker
            .release_presenter(&internal_lease, now_ms);
        if result.is_ok() {
            self.presenter = None;
        }
        self.finish_dialog_mutation(now_ms);
        result
    }

    pub fn presenter_next(
        &mut self,
        wire_lease: &DialogPresenterLeaseV1,
        now_ms: u64,
    ) -> Result<DialogPresenterNextResponseV1, DialogBrokerError> {
        let internal_lease = self.presenter_lease(wire_lease)?;
        let result = self
            .dialog_broker
            .next_presentation(&internal_lease, now_ms);
        self.finish_dialog_mutation(now_ms);
        let presentation = result?.map(|presentation| {
            let handle = presentation.handle.clone();
            let token = presentation.attempt_token;
            self.presenter
                .as_mut()
                .expect("validated presenter session exists")
                .attempts
                .insert(handle.clone(), token);
            DialogPresentationV1 {
                handle: handle.0,
                attempt_token: token.as_u64(),
                dialog: presentation.request.dialog,
                progress: presentation
                    .progress
                    .map(|progress| DialogProgressSnapshotV1 {
                        message: progress.message,
                        progress: progress.progress,
                    }),
                cancel_requested: presentation.cancel_requested,
            }
        });
        Ok(DialogPresenterNextResponseV1 { presentation })
    }

    pub fn presenter_mark_presented(
        &mut self,
        wire_lease: &DialogPresenterLeaseV1,
        handle: &InteractionHandle,
        attempt_token: u64,
        now_ms: u64,
    ) -> Result<(), DialogBrokerError> {
        let (internal_lease, internal_attempt) =
            self.presenter_attempt(wire_lease, handle, attempt_token)?;
        let result =
            self.dialog_broker
                .mark_presented(&internal_lease, handle, internal_attempt, now_ms);
        self.finish_dialog_mutation(now_ms);
        result
    }

    pub fn presenter_resolve(
        &mut self,
        wire_lease: &DialogPresenterLeaseV1,
        handle: &InteractionHandle,
        attempt_token: u64,
        value: DialogValueV1,
        now_ms: u64,
    ) -> Result<(), DialogBrokerError> {
        let (internal_lease, internal_attempt) =
            self.presenter_attempt(wire_lease, handle, attempt_token)?;
        let result =
            self.dialog_broker
                .resolve(&internal_lease, handle, internal_attempt, value, now_ms);
        self.finish_dialog_mutation(now_ms);
        result
    }

    pub fn presenter_fail(
        &mut self,
        wire_lease: &DialogPresenterLeaseV1,
        handle: &InteractionHandle,
        attempt_token: u64,
        now_ms: u64,
    ) -> Result<(), DialogBrokerError> {
        let (internal_lease, internal_attempt) =
            self.presenter_attempt(wire_lease, handle, attempt_token)?;
        let result =
            self.dialog_broker
                .fail_presentation(&internal_lease, handle, internal_attempt, now_ms);
        self.finish_dialog_mutation(now_ms);
        result
    }

    pub fn presenter_progress_cancel(
        &mut self,
        wire_lease: &DialogPresenterLeaseV1,
        handle: &InteractionHandle,
        attempt_token: u64,
        now_ms: u64,
    ) -> Result<(), DialogBrokerError> {
        let (internal_lease, internal_attempt) =
            self.presenter_attempt(wire_lease, handle, attempt_token)?;
        let result = self.dialog_broker.request_progress_cancel(
            &internal_lease,
            handle,
            internal_attempt,
            now_ms,
        );
        self.finish_dialog_mutation(now_ms);
        result
    }

    #[must_use]
    pub fn dialog_next_maintenance_at_ms(&self) -> Option<u64> {
        self.dialog_broker.next_maintenance_at_ms()
    }

    pub fn dialog_maintain(&mut self, now_ms: u64) {
        let _ = self.dialog_broker.gc(now_ms);
        let transitions = self.dialog_broker.drain_transitions();
        self.apply_dialog_transitions(transitions);
    }

    fn presenter_lease(
        &self,
        wire_lease: &DialogPresenterLeaseV1,
    ) -> Result<PresenterLease, DialogBrokerError> {
        let session = self
            .presenter
            .as_ref()
            .ok_or(DialogBrokerError::StaleLease)?;
        if session.wire_lease != *wire_lease {
            return Err(DialogBrokerError::StaleLease);
        }
        Ok(session.internal_lease.clone())
    }

    fn presenter_attempt(
        &self,
        wire_lease: &DialogPresenterLeaseV1,
        handle: &InteractionHandle,
        echoed_attempt: u64,
    ) -> Result<(PresenterLease, PresentationAttemptToken), DialogBrokerError> {
        let internal_lease = self.presenter_lease(wire_lease)?;
        let session = self
            .presenter
            .as_ref()
            .expect("lease validation found a presenter session");
        let internal_attempt = session
            .attempts
            .get(handle)
            .copied()
            .filter(|token| token.as_u64() == echoed_attempt)
            .ok_or(DialogBrokerError::StaleAttempt)?;
        Ok((internal_lease, internal_attempt))
    }

    fn finish_dialog_mutation(&mut self, now_ms: u64) {
        let _ = self.dialog_broker.gc(now_ms);
        let transitions = self.dialog_broker.drain_transitions();
        self.apply_dialog_transitions(transitions);
    }

    fn apply_dialog_transitions(&mut self, batch: DialogTransitionBatch) {
        if batch.overflowed {
            self.rebuild_dialog_projection();
            self.dialog_props_transitions.clear();
            self.dialog_props_resync_needed = true;
            self.bump_props_seq();
            return;
        }

        for transition in batch.transitions {
            let old_progress_fraction = self
                .dialog_records
                .get(transition.handle.as_str())
                .and_then(|record| record.progress_fraction);
            self.update_dialog_projection(&transition);
            let new_progress_fraction = self
                .dialog_records
                .get(transition.handle.as_str())
                .and_then(|record| record.progress_fraction);
            self.prune_presenter_attempt(&transition);
            let seq = self.bump_props_seq();
            if !self.dialog_props_resync_needed {
                self.dialog_props_transitions.push(DialogPropsTransition {
                    seq,
                    handle: transition.handle,
                    old: transition.from,
                    new: transition.to,
                    cause: transition.cause,
                    old_progress_fraction,
                    new_progress_fraction,
                });
            }
        }
    }

    fn update_dialog_projection(&mut self, transition: &DialogTransition) {
        if transition.to.is_none() {
            self.dialog_handles.remove(&transition.handle);
            self.dialog_records.remove(transition.handle.as_str());
            return;
        }
        if let Some(snapshot) = self.dialog_broker.snapshot(&transition.handle) {
            self.dialog_records
                .insert(transition.handle.0.clone(), dialog_props_record(snapshot));
        }
    }

    fn rebuild_dialog_projection(&mut self) {
        let handles: Vec<_> = self.dialog_handles.iter().cloned().collect();
        self.dialog_records.clear();
        for handle in handles {
            if let Some(snapshot) = self.dialog_broker.snapshot(&handle) {
                self.dialog_records
                    .insert(handle.0.clone(), dialog_props_record(snapshot));
            } else {
                self.dialog_handles.remove(&handle);
            }
        }
        if let Some(session) = &mut self.presenter {
            session.attempts.retain(|handle, _| {
                self.dialog_broker.snapshot(handle).is_some_and(|snapshot| {
                    matches!(
                        snapshot.state,
                        DialogStateV1::Presenting
                            | DialogStateV1::Presented
                            | DialogStateV1::CancelRequested
                    )
                })
            });
        }
    }

    fn prune_presenter_attempt(&mut self, transition: &DialogTransition) {
        let retain = matches!(
            transition.to,
            Some(
                DialogStateV1::Presenting
                    | DialogStateV1::Presented
                    | DialogStateV1::CancelRequested
            )
        );
        if !retain && let Some(session) = &mut self.presenter {
            session.attempts.remove(&transition.handle);
        }
    }

    fn bump_props_seq(&mut self) -> u64 {
        self.next_props_seq = self
            .next_props_seq
            .checked_add(1)
            .expect("interaction lifecycle sequence exhausted");
        self.next_props_seq
    }

    /// `interact.notify` queue-time path — apply policy and store `Queued`
    /// without touching noded or D-Bus.
    ///
    /// `fresh` is a freshly minted handle used only for a *new* (non-coalesced)
    /// notification; a dedupe hit reuses the prior handle instead. The returned
    /// job performs registry lookup and sink delivery asynchronously.
    pub fn notify(
        &mut self,
        req: NotifyRequest,
        caller: Caller<'_>,
        ownership: NotifyOwnership,
        now_ms: u64,
        fresh: NotifyHandle,
    ) -> (u8, String, Option<DeliveryJob>) {
        match self.broker.accept_queued(&req, caller, now_ms, fresh) {
            NotifyDecision::Rejected(RejectReason::Invalid(e)) => {
                validation_denied_tuple("invalid notify", &e)
            }
            NotifyDecision::Rejected(RejectReason::UnregisteredDispatch { service, .. }) => {
                err_tuple(&format!("notify dispatch target not registered: {service}"))
            }
            NotifyDecision::Throttled { origin } => {
                eprintln!("cosmix-interactd: [notify] THROTTLED origin={origin} (rate limit)");
                (
                    0,
                    json!({ "throttled": true, "origin": origin }).to_string(),
                    None,
                )
            }
            NotifyDecision::Deliver { record, replaces } => {
                let ownership = match self.owners.get(record.handle.as_str()) {
                    Some(existing) => {
                        let token_matches = ownership
                            .supplied_token
                            .as_ref()
                            .is_some_and(|token| owner_token_matches(&existing.token, token));
                        if existing.caller != ownership.caller || !token_matches {
                            return ownership_denied_tuple(record.handle.as_str());
                        }
                        existing.clone()
                    }
                    None if replaces.is_some() => {
                        return ownership_denied_tuple(record.handle.as_str());
                    }
                    None => HandleOwnership {
                        caller: ownership.caller,
                        token: ownership.fresh_token,
                    },
                };
                let urgency = record.urgency_override.unwrap_or(req.urgency);
                let origin = record.origin.clone();
                let handle = record.handle.0.clone();
                let revision = self.bump_revision(&handle);
                let response = NotifyResponse {
                    handle: record.handle.clone(),
                    owner_token: ownership.token.clone(),
                };
                self.owners.insert(handle.clone(), ownership);
                let old_state = self
                    .records
                    .insert(handle.clone(), record)
                    .map(|previous| previous.state);
                self.record_props_transition(&handle, old_state, NotifyState::Queued);
                let job = DeliveryJob {
                    handle: NotifyHandle(handle),
                    revision,
                    request: req,
                    replaces,
                    kind: DeliveryKind::Notify,
                    origin,
                    urgency,
                };
                (
                    0,
                    serde_json::to_string(&response).expect("notify response is serializable"),
                    Some(job),
                )
            }
        }
    }

    /// Whether a delivery job is still the current non-terminal revision.
    pub fn is_current(&self, job: &DeliveryJob) -> bool {
        self.revisions.get(job.handle.as_str()) == Some(&job.revision)
            && self
                .records
                .get(job.handle.as_str())
                .is_some_and(|record| !record.state.is_terminal())
    }

    /// Resolve one asynchronous delivery attempt and complete the generation
    /// handshake. A pre-route signal is resolved only after the exact route is
    /// installed; a stale completion cannot rewrite or invoke a newer delivery.
    pub fn complete_delivery(
        &mut self,
        job: &DeliveryJob,
        completion: DeliveryCompletion,
    ) -> CompletionResult {
        if !self.is_current(job) {
            return CompletionResult::stale();
        }
        let terminal = match completion {
            DeliveryCompletion::Shown(fd_id) => {
                let previous_route = self.routes.remove(job.handle.as_str());
                let old_state = {
                    let record = self
                        .records
                        .get_mut(job.handle.as_str())
                        .expect("current delivery has a record");
                    let old_state = record.state;
                    record.state = NotifyState::Shown;
                    record.summary = job.request.summary.clone();
                    record.effective_urgency = job.urgency;
                    old_state
                };
                self.record_props_transition(
                    job.handle.as_str(),
                    Some(old_state),
                    NotifyState::Shown,
                );
                if let Some(fd_id) = fd_id {
                    let dispatch = match job.kind {
                        DeliveryKind::Notify => job
                            .request
                            .actions
                            .iter()
                            .filter_map(|action| {
                                action
                                    .on_invoke
                                    .as_ref()
                                    .map(|target| (action.key.clone(), target.clone()))
                            })
                            .collect(),
                        DeliveryKind::Update => previous_route
                            .map(|route| route.dispatch)
                            .unwrap_or_default(),
                    };
                    self.routes.insert(
                        job.handle.0.clone(),
                        Route {
                            revision: job.revision,
                            fd_id,
                            dispatch,
                        },
                    );
                }
                self.pending_signals
                    .remove(&(job.handle.0.clone(), job.revision))
                    .and_then(|signal| {
                        self.resolve_signal(job.handle.as_str(), job.revision, signal)
                    })
            }
            DeliveryCompletion::Failed => {
                self.pending_signals
                    .remove(&(job.handle.0.clone(), job.revision));
                self.mark_terminal(job.handle.as_str(), NotifyState::Failed);
                None
            }
        };
        CompletionResult::applied(terminal)
    }

    /// Offer a generation-bound desktop signal. Events that arrive before their
    /// route are buffered; events from superseded generations are rejected.
    pub fn on_sink_signal(
        &mut self,
        handle: &NotifyHandle,
        revision: u64,
        signal: DeliverySignal,
    ) -> SignalResult {
        if self.revisions.get(handle.as_str()) != Some(&revision) {
            return SignalResult::Stale;
        }
        let Some(record) = self.records.get(handle.as_str()) else {
            return SignalResult::Stale;
        };
        if record.state.is_terminal() {
            return SignalResult::Stale;
        }
        if record.state == NotifyState::Queued {
            self.pending_signals
                .entry((handle.0.clone(), revision))
                .or_insert(signal);
            return SignalResult::Buffered;
        }
        self.resolve_signal(handle.as_str(), revision, signal)
            .map_or(SignalResult::Stale, SignalResult::Resolved)
    }

    fn resolve_signal(
        &mut self,
        handle: &str,
        revision: u64,
        signal: DeliverySignal,
    ) -> Option<TerminalOutcome> {
        let route = self.routes.get(handle)?;
        let signal_fd_id = match &signal {
            DeliverySignal::Action { fd_id, .. } | DeliverySignal::Closed { fd_id, .. } => *fd_id,
        };
        if route.revision != revision || route.fd_id != signal_fd_id {
            return None;
        }
        let route = self.routes.remove(handle)?;
        match signal {
            DeliverySignal::Action { key, .. } => {
                let dispatch = route.dispatch.get(&key).and_then(|dispatch| {
                    self.owners.get(handle).map(|ownership| DispatchOrder {
                        service: dispatch.service.clone(),
                        verb: dispatch.verb.clone(),
                        handle: handle.to_string(),
                        key,
                        requested_by: ownership.caller.service.clone(),
                    })
                });
                self.mark_terminal(handle, NotifyState::ActionInvoked);
                Some(TerminalOutcome::Action(ActionOutcome { dispatch }))
            }
            DeliverySignal::Closed { kind, .. } => {
                let state = match kind {
                    ClosedKind::Expired => NotifyState::Expired,
                    ClosedKind::Dismissed => NotifyState::Dismissed,
                };
                self.mark_terminal(handle, state);
                Some(TerminalOutcome::Closed)
            }
        }
    }

    /// `interact.update` — re-render a live notification in place. Not a broker
    /// decision: provenance stays the record's broker-stamped origin. The
    /// request consumes the same origin bucket as new/replacement notifies and
    /// returns `Queued`; sink resolution happens on the delivery worker.
    pub fn update(
        &mut self,
        owner: &OwnerId,
        owner_token: &OwnerToken,
        handle: &str,
        req: NotifyRequest,
        now_ms: u64,
    ) -> (u8, String, Option<DeliveryJob>) {
        if !self.records.contains_key(handle) {
            return with_no_job(err(&format!("unknown handle: {handle}")));
        }
        if !self.owns(handle, owner, owner_token) {
            return with_no_job(ownership_denied(handle));
        }
        if let Err(e) = req.validate() {
            return validation_denied_with_no_job("invalid update", &e);
        }
        if !req.actions.is_empty() {
            return with_no_job(err(
                "notify.v1: actions are set at notify time only; interact.update cannot change them",
            ));
        }
        let record = self
            .records
            .get(handle)
            .expect("record existence checked above");
        if record.state.is_terminal() {
            return with_no_job(err(&format!(
                "cannot update a {:?} notification",
                record.state
            )));
        }
        let origin = record.origin.clone();
        let current_state = record.state;
        let urgency = record.urgency_override.unwrap_or(req.urgency);
        if !self.broker.try_consume(&origin, now_ms) {
            eprintln!(
                "cosmix-interactd: [notify] THROTTLED origin={origin} handle={handle} (update rate limit)"
            );
            return (
                0,
                json!({ "handle": handle, "throttled": true, "state": current_state }).to_string(),
                None,
            );
        }
        let revision = self.bump_revision(handle);
        let old_state = {
            let record = self
                .records
                .get_mut(handle)
                .expect("record existence checked above");
            let old_state = record.state;
            record.summary = req.summary.clone();
            record.effective_urgency = urgency;
            record.state = NotifyState::Queued;
            old_state
        };
        self.record_props_transition(handle, Some(old_state), NotifyState::Queued);
        let handle = NotifyHandle(handle.to_string());
        let response = NotifyMutationResponse {
            handle: handle.clone(),
            state: NotifyState::Queued,
        };
        let job = DeliveryJob {
            handle: handle.clone(),
            revision,
            request: req,
            replaces: Some(handle),
            kind: DeliveryKind::Update,
            origin,
            urgency,
        };
        (
            0,
            serde_json::to_string(&response).expect("update response is serializable"),
            Some(job),
        )
    }

    /// `interact.dismiss` — close a live notification and release its broker
    /// bookkeeping. Idempotent: dismissing an already-terminal handle is a
    /// success no-op.
    pub fn dismiss(
        &mut self,
        owner: &OwnerId,
        owner_token: &OwnerToken,
        handle: &str,
    ) -> (u8, String, Option<NotifyHandle>) {
        if !self.records.contains_key(handle) {
            return with_no_job(err(&format!("unknown handle: {handle}")));
        }
        if !self.owns(handle, owner, owner_token) {
            return with_no_job(ownership_denied(handle));
        }
        let record = self
            .records
            .get(handle)
            .expect("record existence checked above");
        if record.state.is_terminal() {
            return (
                0,
                serde_json::to_string(&NotifyMutationResponse {
                    handle: NotifyHandle(handle.to_string()),
                    state: record.state,
                })
                .expect("dismiss response is serializable"),
                None,
            );
        }
        let h = NotifyHandle(handle.to_string());
        self.bump_revision(handle);
        self.mark_terminal(handle, NotifyState::Dismissed);
        (
            0,
            serde_json::to_string(&NotifyMutationResponse {
                handle: h.clone(),
                state: NotifyState::Dismissed,
            })
            .expect("dismiss response is serializable"),
            Some(h),
        )
    }

    fn bump_revision(&mut self, handle: &str) -> u64 {
        self.pending_signals
            .retain(|(pending_handle, _), _| pending_handle != handle);
        let revision = self.revisions.entry(handle.to_string()).or_default();
        *revision = revision.saturating_add(1);
        *revision
    }

    fn mark_terminal(&mut self, handle: &str, state: NotifyState) {
        let old_state = self.records.get_mut(handle).and_then(|record| {
            if record.state.is_terminal() {
                None
            } else {
                let old = record.state;
                record.state = state;
                Some(old)
            }
        });
        let Some(old_state) = old_state else {
            return;
        };
        self.record_props_transition(handle, Some(old_state), state);
        self.broker.retire(&NotifyHandle(handle.to_string()));
        self.routes.remove(handle);
        self.pending_signals
            .retain(|(pending_handle, _), _| pending_handle != handle);
        self.terminal_order.push_back(handle.to_string());
        while self.terminal_order.len() > TERMINAL_RECORD_CAP {
            if let Some(evicted) = self.terminal_order.pop_front() {
                self.records.remove(&evicted);
                self.owners.remove(&evicted);
                self.revisions.remove(&evicted);
                self.pending_signals
                    .retain(|(pending_handle, _), _| pending_handle != &evicted);
            }
        }
    }

    fn owns(&self, handle: &str, owner: &OwnerId, owner_token: &OwnerToken) -> bool {
        self.owners.get(handle).is_some_and(|ownership| {
            let token_matches = owner_token_matches(&ownership.token, owner_token);
            ownership.caller == *owner && token_matches
        })
    }

    fn record_props_transition(
        &mut self,
        handle: &str,
        old: Option<NotifyState>,
        new: NotifyState,
    ) {
        if old == Some(new) {
            return;
        }
        let seq = self.bump_props_seq();
        self.props_transitions.push(PropsStateTransition {
            seq,
            handle: NotifyHandle(handle.to_string()),
            old,
            new,
        });
    }
}

fn draw_dialog_instance_epoch() -> NonZeroU64 {
    let mut bytes = [0_u8; std::mem::size_of::<u64>()];
    loop {
        getrandom::fill(&mut bytes).expect("OS CSPRNG unavailable for dialog instance epoch");
        if let Some(epoch) = NonZeroU64::new(u64::from_ne_bytes(bytes)) {
            return epoch;
        }
    }
}

fn dialog_props_record(snapshot: DialogSnapshot) -> DialogPropsRecord {
    let progress_fraction = snapshot
        .progress
        .and_then(|snapshot| match snapshot.progress {
            DialogProgressValueV1::Determinate { current, total } => {
                Some(current as f64 / total as f64)
            }
            DialogProgressValueV1::Indeterminate {} => None,
        });
    DialogPropsRecord {
        handle: snapshot.handle,
        origin: snapshot.owner_service,
        state: snapshot.state,
        created_at_ms: snapshot.created_at_ms,
        progress_fraction,
    }
}

fn with_no_job<T>((rc, body): (u8, String)) -> (u8, String, Option<T>) {
    (rc, body, None)
}

const OWNER_TOKEN_LEN: usize = 33;

/// Compare the fixed-width `o` + UUID token without data-dependent early exit.
/// Invalid-width candidates are padded/truncated into the same 33-byte compare
/// and fail through the accumulated length bit.
fn owner_token_matches(expected: &OwnerToken, supplied: &OwnerToken) -> bool {
    let expected_bytes: &[u8; OWNER_TOKEN_LEN] = expected
        .as_str()
        .as_bytes()
        .try_into()
        .expect("server-minted owner token has fixed width");
    let supplied_bytes = supplied.as_str().as_bytes();
    let mut fixed = [0_u8; OWNER_TOKEN_LEN];
    let copy_len = supplied_bytes.len().min(OWNER_TOKEN_LEN);
    fixed[..copy_len].copy_from_slice(&supplied_bytes[..copy_len]);
    let same_length = u8::from(supplied_bytes.len() == OWNER_TOKEN_LEN);
    bool::from(expected_bytes.ct_eq(&fixed) & subtle::Choice::from(same_length))
}

/// [`crate::err`] widened to the 3-tuple `notify` returns (never a listener).
fn err_tuple<T>(msg: &str) -> (u8, String, Option<T>) {
    let (rc, body) = err(msg);
    (rc, body, None)
}

fn ownership_denied(handle: &str) -> (u8, String) {
    crate::app_error_with_handle(
        "ownership_denied",
        "notification mutation requires the creating caller's owner token",
        handle,
    )
}

fn ownership_denied_tuple<T>(handle: &str) -> (u8, String, Option<T>) {
    let (rc, body) = ownership_denied(handle);
    (rc, body, None)
}

fn validation_denied_with_no_job<T>(
    context: &str,
    error: &ValidationError,
) -> (u8, String, Option<T>) {
    with_no_job(crate::app_error(
        error.code(),
        &format!("{context}: {error}"),
    ))
}

fn validation_denied_tuple<T>(context: &str, error: &ValidationError) -> (u8, String, Option<T>) {
    validation_denied_with_no_job(context, error)
}

#[cfg(test)]
mod tests {
    use super::*;

    use cosmix_interaction_broker::{
        DialogQueueLimits, DialogRetentionPolicy, RateConfig, RateLimiter,
    };
    use cosmix_interaction_schema::{
        DialogCommonV1, DialogRequestV1, DialogSeverityV1, Dispatch, NotifyAction, Urgency,
    };
    use serde_json::Value;

    fn state() -> InteractState {
        InteractState::new()
    }

    fn owner(service: &str) -> OwnerId {
        OwnerId::local(service)
    }

    fn token(value: &str) -> OwnerToken {
        assert!(value.len() <= 32);
        OwnerToken(format!("o{value:0<32}"))
    }

    fn handle_of(body: &str) -> String {
        let v: Value = serde_json::from_str(body).unwrap();
        v["handle"].as_str().unwrap().to_string()
    }

    fn owner_token_of(body: &str) -> OwnerToken {
        serde_json::from_str::<NotifyResponse>(body)
            .unwrap()
            .owner_token
    }

    #[test]
    fn owner_token_comparison_is_fixed_width_and_exact() {
        let expected = token("expected");
        assert!(owner_token_matches(&expected, &expected));
        assert!(!owner_token_matches(&expected, &token("different")));
        assert!(!owner_token_matches(&expected, &OwnerToken("short".into())));
        assert!(!owner_token_matches(
            &expected,
            &OwnerToken("o000000000000000000000000000000000".into())
        ));
    }

    #[test]
    fn props_transition_log_covers_every_lifecycle_state() {
        for terminal in [
            NotifyState::Dismissed,
            NotifyState::Expired,
            NotifyState::ActionInvoked,
            NotifyState::Failed,
        ] {
            let mut st = state();
            let (_, _, job) = st.notify(
                NotifyRequest::new("transition"),
                Caller::local("musicd"),
                NotifyOwnership::new(owner("musicd"), None, token("t1")),
                1_000,
                NotifyHandle("n1".into()),
            );
            let job = job.unwrap();
            finish_shown(&mut st, &job, Some(7));
            st.mark_terminal("n1", terminal);

            assert_eq!(
                st.take_props_transitions(),
                [
                    PropsStateTransition {
                        seq: 1,
                        handle: NotifyHandle("n1".into()),
                        old: None,
                        new: NotifyState::Queued,
                    },
                    PropsStateTransition {
                        seq: 2,
                        handle: NotifyHandle("n1".into()),
                        old: Some(NotifyState::Queued),
                        new: NotifyState::Shown,
                    },
                    PropsStateTransition {
                        seq: 3,
                        handle: NotifyHandle("n1".into()),
                        old: Some(NotifyState::Shown),
                        new: terminal,
                    },
                ]
            );
        }
    }

    fn finish_shown(state: &mut InteractState, job: &DeliveryJob, fd_id: Option<u32>) {
        assert!(
            state
                .complete_delivery(job, DeliveryCompletion::Shown(fd_id))
                .current
        );
    }

    #[test]
    fn notify_delivers_and_stores_shown() {
        let mut st = state();
        let (rc, body, job) = st.notify(
            NotifyRequest::new("Build finished"),
            Caller::local("musicd"),
            NotifyOwnership::new(owner("musicd"), None, token("t1")),
            1_000,
            NotifyHandle("n1".into()),
        );
        assert_eq!(rc, 0);
        let h = handle_of(&body);
        assert_eq!(h, "n1");
        assert_eq!(st.records()[&h].state, NotifyState::Queued);
        finish_shown(&mut st, &job.unwrap(), Some(1));
        assert_eq!(st.records()[&h].state, NotifyState::Shown);
        assert_eq!(st.records()[&h].origin, "musicd");
    }

    #[test]
    fn action_click_dispatches_and_marks_invoked() {
        let mut st = state();
        let mut req = NotifyRequest::new("Deploy done");
        req.actions = vec![NotifyAction {
            key: "open".into(),
            label: "Open".into(),
            on_invoke: Some(Dispatch {
                service: "filemgr".into(),
                verb: "app.reveal".into(),
            }),
        }];
        let (rc, body, job) = st.notify(
            req,
            Caller::local("cid"),
            NotifyOwnership::new(owner("cid"), None, token("t1")),
            1_000,
            NotifyHandle("n1".into()),
        );
        assert_eq!(rc, 0);
        let h = handle_of(&body);
        let fd = 7;
        let job = job.unwrap();
        finish_shown(&mut st, &job, Some(fd));
        // The button click resolves to its dispatch and marks the record done.
        let SignalResult::Resolved(TerminalOutcome::Action(outcome)) = st.on_sink_signal(
            &job.handle,
            job.revision,
            DeliverySignal::Action {
                fd_id: fd,
                key: "open".into(),
            },
        ) else {
            panic!("open should resolve through the current generation");
        };
        let order = outcome.dispatch.expect("action has a dispatch target");
        assert_eq!(order.service, "filemgr");
        assert_eq!(order.verb, "app.reveal");
        assert_eq!(order.handle, h);
        assert_eq!(order.key, "open");
        assert_eq!(order.requested_by, "cid");
        assert_eq!(st.records()[&h].state, NotifyState::ActionInvoked);
        // Route consumed → a second click on the (now gone) toast is a no-op.
        assert!(matches!(
            st.on_sink_signal(
                &job.handle,
                job.revision,
                DeliverySignal::Action {
                    fd_id: fd,
                    key: "open".into()
                }
            ),
            SignalResult::Stale
        ));
    }

    #[test]
    fn keyless_action_marks_invoked_without_a_dispatch() {
        let mut st = state();
        let mut req = NotifyRequest::new("Ping");
        req.actions = vec![NotifyAction {
            key: "ok".into(),
            label: "OK".into(),
            on_invoke: None,
        }];
        let (_, body, job) = st.notify(
            req,
            Caller::local("cid"),
            NotifyOwnership::new(owner("cid"), None, token("t1")),
            1_000,
            NotifyHandle("n1".into()),
        );
        let h = handle_of(&body);
        let fd = 8;
        let job = job.unwrap();
        finish_shown(&mut st, &job, Some(fd));
        let SignalResult::Resolved(TerminalOutcome::Action(outcome)) = st.on_sink_signal(
            &job.handle,
            job.revision,
            DeliverySignal::Action {
                fd_id: fd,
                key: "ok".into(),
            },
        ) else {
            panic!("keyless action should still resolve");
        };
        assert!(outcome.dispatch.is_none(), "no on_invoke → no order");
        assert_eq!(st.records()[&h].state, NotifyState::ActionInvoked);
    }

    #[test]
    fn dispatch_target_validation_is_deferred_until_delivery() {
        let mut st = state();
        let mut req = NotifyRequest::new("Deploy done");
        req.actions = vec![NotifyAction {
            key: "open".into(),
            label: "Open".into(),
            on_invoke: Some(Dispatch {
                service: "ghost".into(),
                verb: "app.open".into(),
            }),
        }];
        let (rc, body, job) = st.notify(
            req,
            Caller::local("cid"),
            NotifyOwnership::new(owner("cid"), None, token("t1")),
            1_000,
            NotifyHandle("n1".into()),
        );
        assert_eq!(rc, 0, "{body}");
        assert!(job.is_some());
        assert_eq!(st.records()["n1"].state, NotifyState::Queued);
    }

    #[test]
    fn close_signal_marks_terminal_idempotently() {
        let mut st = state();
        let (_, body, job) = st.notify(
            NotifyRequest::new("Toast"),
            Caller::local("cid"),
            NotifyOwnership::new(owner("cid"), None, token("t1")),
            1_000,
            NotifyHandle("n1".into()),
        );
        let h = handle_of(&body);
        let fd = 9;
        let job = job.unwrap();
        finish_shown(&mut st, &job, Some(fd));
        assert!(matches!(
            st.on_sink_signal(
                &job.handle,
                job.revision,
                DeliverySignal::Closed {
                    fd_id: fd,
                    kind: ClosedKind::Expired
                }
            ),
            SignalResult::Resolved(TerminalOutcome::Closed)
        ));
        assert_eq!(st.records()[&h].state, NotifyState::Expired);
        // The route is gone → a second close (or a stale signal) is a no-op.
        assert!(matches!(
            st.on_sink_signal(
                &job.handle,
                job.revision,
                DeliverySignal::Closed {
                    fd_id: fd,
                    kind: ClosedKind::Dismissed
                }
            ),
            SignalResult::Stale
        ));
        assert_eq!(st.records()[&h].state, NotifyState::Expired);
    }

    #[test]
    fn pre_route_close_is_buffered_and_resolved_by_delivery_handshake() {
        let mut st = state();
        let (_, _, job) = st.notify(
            NotifyRequest::new("Very short toast"),
            Caller::local("cid"),
            NotifyOwnership::new(owner("cid"), None, token("t1")),
            1_000,
            NotifyHandle("n1".into()),
        );
        let job = job.unwrap();
        assert!(matches!(
            st.on_sink_signal(
                &job.handle,
                job.revision,
                DeliverySignal::Closed {
                    fd_id: 44,
                    kind: ClosedKind::Expired,
                },
            ),
            SignalResult::Buffered
        ));
        assert_eq!(st.records()["n1"].state, NotifyState::Queued);

        let completion = st.complete_delivery(&job, DeliveryCompletion::Shown(Some(44)));
        assert!(completion.current);
        assert!(matches!(completion.terminal, Some(TerminalOutcome::Closed)));
        assert_eq!(st.records()["n1"].state, NotifyState::Expired);
    }

    #[test]
    fn dedupe_replacement_rekeys_route_and_rejects_old_generation() {
        let mut st = state();
        let mut first = NotifyRequest::new("First");
        first.dedupe_key = Some("job".into());
        first.actions.push(NotifyAction {
            key: "open".into(),
            label: "Open old".into(),
            on_invoke: Some(Dispatch {
                service: "old-app".into(),
                verb: "app.open".into(),
            }),
        });
        let (_, body, first_job) = st.notify(
            first,
            Caller::local("filesd"),
            NotifyOwnership::new(owner("filesd"), None, token("t1")),
            1_000,
            NotifyHandle("n1".into()),
        );
        let first_job = first_job.unwrap();
        finish_shown(&mut st, &first_job, Some(51));

        let mut replacement = NotifyRequest::new("Replacement");
        replacement.dedupe_key = Some("job".into());
        replacement.actions.push(NotifyAction {
            key: "open".into(),
            label: "Open new".into(),
            on_invoke: Some(Dispatch {
                service: "new-app".into(),
                verb: "app.reveal".into(),
            }),
        });
        let (_, _, replacement_job) = st.notify(
            replacement,
            Caller::local("filesd"),
            NotifyOwnership::new(
                owner("filesd"),
                Some(owner_token_of(&body)),
                token("unused"),
            ),
            2_000,
            NotifyHandle("n2".into()),
        );
        let replacement_job = replacement_job.unwrap();
        assert!(matches!(
            st.on_sink_signal(
                &first_job.handle,
                first_job.revision,
                DeliverySignal::Closed {
                    fd_id: 51,
                    kind: ClosedKind::Dismissed,
                },
            ),
            SignalResult::Stale
        ));
        finish_shown(&mut st, &replacement_job, Some(52));
        assert_eq!(st.records()["n1"].state, NotifyState::Shown);
        assert!(matches!(
            st.on_sink_signal(
                &first_job.handle,
                first_job.revision,
                DeliverySignal::Action {
                    fd_id: 51,
                    key: "open".into(),
                },
            ),
            SignalResult::Stale
        ));
        let SignalResult::Resolved(TerminalOutcome::Action(outcome)) = st.on_sink_signal(
            &replacement_job.handle,
            replacement_job.revision,
            DeliverySignal::Action {
                fd_id: 52,
                key: "open".into(),
            },
        ) else {
            panic!("replacement generation should own the route");
        };
        let order = outcome.dispatch.unwrap();
        assert_eq!(order.service, "new-app");
        assert_eq!(order.verb, "app.reveal");
    }

    #[test]
    fn empty_summary_rejected() {
        let mut st = state();
        let (rc, body, listen) = st.notify(
            NotifyRequest::new("   "),
            Caller::local("cid"),
            NotifyOwnership::new(owner("cid"), None, token("t1")),
            1_000,
            NotifyHandle("n1".into()),
        );
        assert_eq!(rc, 10);
        assert_eq!(listen, None);
        assert!(body.contains("invalid notify"));
    }

    #[test]
    fn dedupe_key_coalesces_onto_same_handle() {
        let mut st = state();
        let mut a = NotifyRequest::new("Uploading… 10%");
        a.dedupe_key = Some("upload".into());
        let (_, body_a, job_a) = st.notify(
            a,
            Caller::local("filesd"),
            NotifyOwnership::new(owner("filesd"), None, token("t1")),
            1_000,
            NotifyHandle("n1".into()),
        );
        finish_shown(&mut st, &job_a.unwrap(), Some(10));
        let owner_token = owner_token_of(&body_a);

        let mut b = NotifyRequest::new("Uploading… 80%");
        b.dedupe_key = Some("upload".into());
        // A different fresh handle is offered, but the dedupe hit must reuse n1.
        let (_, body_b, job_b) = st.notify(
            b,
            Caller::local("filesd"),
            NotifyOwnership::new(owner("filesd"), Some(owner_token), token("unused")),
            1_100,
            NotifyHandle("n2".into()),
        );

        assert_eq!(handle_of(&body_a), "n1");
        assert_eq!(handle_of(&body_b), "n1");
        assert_eq!(st.records().len(), 1);
        let job_b = job_b.unwrap();
        assert_eq!(job_b.replaces, Some(NotifyHandle("n1".into())));
        assert_eq!(st.records()["n1"].state, NotifyState::Queued);
        finish_shown(&mut st, &job_b, Some(10));
    }

    #[test]
    fn dedupe_replacement_requires_the_existing_owner_token() {
        let mut st = state();
        let mut first = NotifyRequest::new("first");
        first.dedupe_key = Some("job".into());
        let (_, body, _) = st.notify(
            first,
            Caller::anonymous(),
            NotifyOwnership::new(owner(""), None, token("secret")),
            1_000,
            NotifyHandle("n1".into()),
        );
        assert_eq!(owner_token_of(&body), token("secret"));

        let mut takeover = NotifyRequest::new("takeover");
        takeover.dedupe_key = Some("job".into());
        let (rc, denied, listen) = st.notify(
            takeover,
            Caller::anonymous(),
            NotifyOwnership::new(owner(""), None, token("attacker")),
            1_100,
            NotifyHandle("n2".into()),
        );
        assert_eq!(rc, 10);
        assert!(denied.contains("ownership_denied"));
        assert_eq!(listen, None);
        assert_eq!(st.records()["n1"].summary, "first");
    }

    #[test]
    fn observed_token_cannot_cross_service_on_dedupe_replacement() {
        let mut st = state();
        let mut first = NotifyRequest::new("first");
        first.dedupe_key = Some("job".into());
        let (_, body, _) = st.notify(
            first,
            Caller::anonymous(),
            NotifyOwnership::new(owner("creator"), None, token("observed")),
            1_000,
            NotifyHandle("n1".into()),
        );

        let mut takeover = NotifyRequest::new("takeover");
        takeover.dedupe_key = Some("job".into());
        let (rc, denied, job) = st.notify(
            takeover,
            Caller::anonymous(),
            NotifyOwnership::new(
                owner("attacker"),
                Some(owner_token_of(&body)),
                token("unused"),
            ),
            1_100,
            NotifyHandle("n2".into()),
        );
        assert_eq!(rc, 10);
        assert!(denied.contains("ownership_denied"));
        assert!(job.is_none());
        assert_eq!(st.records()["n1"].summary, "first");
    }

    #[test]
    fn rate_limit_throttles_after_burst() {
        let mut st = state();
        // Default budget: burst 5. The 6th distinct (no dedupe) notify at the
        // same instant is throttled.
        for i in 0..5 {
            let (rc, body, _) = st.notify(
                NotifyRequest::new(format!("msg {i}")),
                Caller::local("noisy"),
                NotifyOwnership::new(owner("noisy"), None, token(&format!("t{i}"))),
                1_000,
                NotifyHandle(format!("n{i}")),
            );
            assert_eq!(rc, 0, "burst notify {i} should pass: {body}");
        }
        let (rc, body, listen) = st.notify(
            NotifyRequest::new("overflow"),
            Caller::local("noisy"),
            NotifyOwnership::new(owner("noisy"), None, token("t5")),
            1_000,
            NotifyHandle("n5".into()),
        );
        assert_eq!(rc, 0, "throttle is a soft outcome, not an error");
        assert_eq!(
            listen, None,
            "a throttled notify shows nothing to listen for"
        );
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["throttled"], true);
        assert_eq!(v["origin"], "noisy");
    }

    #[test]
    fn update_shares_the_notify_rate_bucket() {
        let mut st = state();
        let mut first_body = String::new();
        for i in 0..5 {
            let (rc, body, job) = st.notify(
                NotifyRequest::new(format!("msg {i}")),
                Caller::local("noisy"),
                NotifyOwnership::new(owner("noisy"), None, token(&format!("t{i}"))),
                1_000,
                NotifyHandle(format!("n{i}")),
            );
            assert_eq!(rc, 0, "{body}");
            assert!(job.is_some());
            if i == 0 {
                first_body = body;
            }
        }
        let owner_token = owner_token_of(&first_body);
        let (rc, body, job) = st.update(
            &owner("noisy"),
            &owner_token,
            "n0",
            NotifyRequest::new("bypass attempt"),
            1_000,
        );
        assert_eq!(rc, 0);
        assert!(job.is_none());
        assert_eq!(
            serde_json::from_str::<Value>(&body).unwrap()["throttled"],
            true
        );
        assert_eq!(st.records()["n0"].summary, "msg 0");
    }

    #[test]
    fn failed_delivery_retires_dedupe_slot() {
        let mut st = state();
        let mut req = NotifyRequest::new("first");
        req.dedupe_key = Some("job".into());
        let (_, _, first_job) = st.notify(
            req.clone(),
            Caller::local("filesd"),
            NotifyOwnership::new(owner("filesd"), None, token("first")),
            1_000,
            NotifyHandle("n1".into()),
        );
        let first_job = first_job.unwrap();
        assert!(
            st.complete_delivery(&first_job, DeliveryCompletion::Failed)
                .current
        );
        assert_eq!(st.records()["n1"].state, NotifyState::Failed);

        let (_, _, second_job) = st.notify(
            req,
            Caller::local("filesd"),
            NotifyOwnership::new(owner("filesd"), None, token("second")),
            1_000,
            NotifyHandle("n2".into()),
        );
        let second_job = second_job.unwrap();
        assert_eq!(second_job.handle, NotifyHandle("n2".into()));
        assert_eq!(second_job.replaces, None);
    }

    #[test]
    fn terminal_record_retention_evicts_oldest_only() {
        let mut st = state();
        for i in 0..=TERMINAL_RECORD_CAP {
            let service = format!("s{i}");
            let handle = format!("n{i}");
            let (_, _, job) = st.notify(
                NotifyRequest::new(format!("message {i}")),
                Caller::local(&service),
                NotifyOwnership::new(owner(&service), None, token("stable")),
                1_000 + i as u64 * 500,
                NotifyHandle(handle),
            );
            assert!(
                st.complete_delivery(&job.unwrap(), DeliveryCompletion::Failed)
                    .current
            );
        }
        assert_eq!(st.records().len(), TERMINAL_RECORD_CAP);
        assert!(!st.records().contains_key("n0"));
        assert!(
            st.records()
                .contains_key(&format!("n{TERMINAL_RECORD_CAP}"))
        );
    }

    #[test]
    fn remote_critical_is_clamped_to_normal() {
        let mut st = state();
        let mut req = NotifyRequest::new("Peer alert");
        req.urgency = Urgency::Critical;
        let (rc, body, job) = st.notify(
            req,
            Caller::remote("peer"),
            NotifyOwnership::new(owner("peer"), None, token("t1")),
            1_000,
            NotifyHandle("n1".into()),
        );
        assert_eq!(rc, 0);
        let h = handle_of(&body);
        assert_eq!(st.records()[&h].urgency_override, Some(Urgency::Normal));
        assert_eq!(st.records()[&h].effective_urgency, Urgency::Normal);
        assert_eq!(job.unwrap().view().urgency, Urgency::Normal);
    }

    #[test]
    fn dismiss_closes_and_marks_terminal() {
        let mut st = state();
        let (_, body, _) = st.notify(
            NotifyRequest::new("Job running"),
            Caller::local("cid"),
            NotifyOwnership::new(owner("cid"), None, token("t1")),
            1_000,
            NotifyHandle("n1".into()),
        );
        let h = handle_of(&body);
        let owner_token = owner_token_of(&body);
        let (rc, _, close) = st.dismiss(&owner("cid"), &owner_token, &h);
        assert_eq!(rc, 0);
        assert_eq!(close, Some(NotifyHandle(h.clone())));
        assert_eq!(st.records()[&h].state, NotifyState::Dismissed);
        // dismiss is idempotent
        let (rc2, _, close2) = st.dismiss(&owner("cid"), &owner_token, &h);
        assert_eq!(rc2, 0);
        assert_eq!(close2, None);
    }

    #[test]
    fn dismiss_unknown_handle_errors() {
        let mut st = state();
        let (rc, body, close) = st.dismiss(&owner("cid"), &token("token"), "nope");
        assert_eq!(rc, 10);
        assert_eq!(close, None);
        assert!(body.contains("unknown handle"));
    }

    #[test]
    fn update_rerenders_live_notification() {
        let mut st = state();
        let (_, body, initial_job) = st.notify(
            NotifyRequest::new("Downloading…"),
            Caller::local("filesd"),
            NotifyOwnership::new(owner("filesd"), None, token("t1")),
            1_000,
            NotifyHandle("n1".into()),
        );
        let h = handle_of(&body);
        finish_shown(&mut st, &initial_job.unwrap(), Some(11));
        let owner_token = owner_token_of(&body);
        let (rc, response, update_job) = st.update(
            &owner("filesd"),
            &owner_token,
            &h,
            NotifyRequest::new("Download complete"),
            2_000,
        );
        assert_eq!(rc, 0, "{response}");
        assert_eq!(st.records()[&h].summary, "Download complete");
        assert_eq!(st.records()[&h].state, NotifyState::Queued);
        let update_job = update_job.unwrap();
        assert_eq!(update_job.kind, DeliveryKind::Update);
        finish_shown(&mut st, &update_job, Some(11));
        assert_eq!(st.records()[&h].state, NotifyState::Shown);
    }

    #[test]
    fn failed_update_is_recorded_as_failed_not_shown() {
        let mut st = state();
        let (_, body, initial_job) = st.notify(
            NotifyRequest::new("Downloading…"),
            Caller::local("filesd"),
            NotifyOwnership::new(owner("filesd"), None, token("t1")),
            1_000,
            NotifyHandle("n1".into()),
        );
        finish_shown(&mut st, &initial_job.unwrap(), Some(11));
        let owner_token = owner_token_of(&body);
        let (rc, response, update_job) = st.update(
            &owner("filesd"),
            &owner_token,
            "n1",
            NotifyRequest::new("Download complete"),
            2_000,
        );
        assert_eq!(rc, 0, "{response}");
        let update_job = update_job.unwrap();
        assert!(
            st.complete_delivery(&update_job, DeliveryCompletion::Failed)
                .current
        );
        assert_eq!(st.records()["n1"].state, NotifyState::Failed);
        assert!(matches!(
            st.on_sink_signal(
                &update_job.handle,
                update_job.revision,
                DeliverySignal::Action {
                    fd_id: 11,
                    key: "default".into()
                }
            ),
            SignalResult::Stale
        ));
    }

    #[test]
    fn update_rekeys_action_route_when_desktop_id_changes() {
        let mut st = state();
        let mut request = NotifyRequest::new("Before");
        request.actions.push(NotifyAction {
            key: "open".into(),
            label: "Open".into(),
            on_invoke: Some(Dispatch {
                service: "filemgr".into(),
                verb: "app.open".into(),
            }),
        });
        let (_, body, initial_job) = st.notify(
            request,
            Caller::local("filesd"),
            NotifyOwnership::new(owner("filesd"), None, token("t1")),
            1_000,
            NotifyHandle("n1".into()),
        );
        finish_shown(&mut st, &initial_job.unwrap(), Some(11));
        let (_, _, update_job) = st.update(
            &owner("filesd"),
            &owner_token_of(&body),
            "n1",
            NotifyRequest::new("After"),
            2_000,
        );
        let update_job = update_job.unwrap();
        finish_shown(&mut st, &update_job, Some(12));

        assert!(matches!(
            st.on_sink_signal(
                &update_job.handle,
                update_job.revision,
                DeliverySignal::Action {
                    fd_id: 11,
                    key: "open".into()
                }
            ),
            SignalResult::Stale
        ));
        let SignalResult::Resolved(TerminalOutcome::Action(outcome)) = st.on_sink_signal(
            &update_job.handle,
            update_job.revision,
            DeliverySignal::Action {
                fd_id: 12,
                key: "open".into(),
            },
        ) else {
            panic!("new desktop id should own the action route");
        };
        let action = outcome.dispatch.unwrap();
        assert_eq!(action.service, "filemgr");
        assert_eq!(action.verb, "app.open");
    }

    #[test]
    fn superseded_delivery_completion_cannot_overwrite_newer_queue_state() {
        let mut st = state();
        let (_, body, first_job) = st.notify(
            NotifyRequest::new("first"),
            Caller::local("filesd"),
            NotifyOwnership::new(owner("filesd"), None, token("t1")),
            1_000,
            NotifyHandle("n1".into()),
        );
        let owner_token = owner_token_of(&body);
        let (_, _, update_job) = st.update(
            &owner("filesd"),
            &owner_token,
            "n1",
            NotifyRequest::new("newer"),
            2_000,
        );
        assert!(
            !st.complete_delivery(&first_job.unwrap(), DeliveryCompletion::Shown(Some(20)))
                .current
        );
        assert_eq!(st.records()["n1"].state, NotifyState::Queued);
        assert_eq!(st.records()["n1"].summary, "newer");
        finish_shown(&mut st, &update_job.unwrap(), Some(21));
    }

    #[test]
    fn non_owner_cannot_update_or_dismiss() {
        let mut st = state();
        let (_, body, job) = st.notify(
            NotifyRequest::new("Downloading…"),
            Caller::local("filesd"),
            NotifyOwnership::new(owner("filesd"), None, token("t1")),
            1_000,
            NotifyHandle("n1".into()),
        );
        let h = handle_of(&body);
        finish_shown(&mut st, &job.unwrap(), Some(12));
        let owner_token = owner_token_of(&body);

        let (update_rc, update_body, update_job) = st.update(
            &owner("otherd"),
            &owner_token,
            &h,
            NotifyRequest::new("forged update"),
            2_000,
        );
        assert_eq!(update_rc, 10);
        assert!(update_job.is_none());
        assert!(update_body.contains("ownership_denied"));
        assert_eq!(st.records()[&h].summary, "Downloading…");

        let (dismiss_rc, dismiss_body, close) =
            st.dismiss(&owner("filesd"), &token("wrong-token"), &h);
        assert_eq!(dismiss_rc, 10);
        assert!(close.is_none());
        assert!(dismiss_body.contains("ownership_denied"));
        assert_eq!(st.records()[&h].state, NotifyState::Shown);
    }

    #[test]
    fn update_rejects_actions() {
        let mut st = state();
        let (_, body, _) = st.notify(
            NotifyRequest::new("x"),
            Caller::local("cid"),
            NotifyOwnership::new(owner("cid"), None, token("t1")),
            1_000,
            NotifyHandle("n1".into()),
        );
        let h = handle_of(&body);
        let owner_token = owner_token_of(&body);
        let mut upd = NotifyRequest::new("y");
        upd.actions = vec![NotifyAction {
            key: "a".into(),
            label: "A".into(),
            on_invoke: None,
        }];
        let (rc, msg, job) = st.update(&owner("cid"), &owner_token, &h, upd, 2_000);
        assert_eq!(rc, 10);
        assert!(job.is_none());
        assert!(msg.contains("actions are set at notify time only"));
    }

    #[test]
    fn update_terminal_is_rejected() {
        let mut st = state();
        let (_, body, _) = st.notify(
            NotifyRequest::new("x"),
            Caller::local("cid"),
            NotifyOwnership::new(owner("cid"), None, token("t1")),
            1_000,
            NotifyHandle("n1".into()),
        );
        let h = handle_of(&body);
        let owner_token = owner_token_of(&body);
        st.dismiss(&owner("cid"), &owner_token, &h);
        let (rc, msg, job) = st.update(
            &owner("cid"),
            &owner_token,
            &h,
            NotifyRequest::new("too late"),
            2_000,
        );
        assert_eq!(rc, 10);
        assert!(job.is_none());
        assert!(msg.contains("cannot update"));
    }

    #[test]
    fn dialog_transition_overflow_rebuilds_projection_and_requests_full_resync() {
        let epoch = NonZeroU64::new(77).unwrap();
        let mut state = InteractState::with_dialog_instance_epoch(epoch);
        let generous = RateConfig {
            capacity: 1_000.0,
            refill_per_sec: 1_000.0,
        };
        state.dialog_broker = DialogBroker::new_with_retention(
            RateLimiter::new_with_global(generous, generous),
            DialogQueueLimits {
                total: 1,
                per_origin: 1,
            },
            DialogRetentionPolicy {
                active: 1,
                terminal: 0,
                terminal_ttl_ms: 60_000,
            },
        )
        .with_instance_epoch(epoch);

        let handle = InteractionHandle("doverflow".into());
        let owner_token = OwnerToken("o00000000000000000000000000000000".into());
        state
            .dialog_open(
                "musicd",
                owner_token.clone(),
                DialogOpenRequestV1 {
                    dialog: DialogRequestV1::Progress {
                        common: DialogCommonV1 {
                            title: "Work".into(),
                            message: Some("Starting".into()),
                            severity: DialogSeverityV1::Info,
                        },
                        progress: DialogProgressValueV1::Determinate {
                            current: 0,
                            total: 10,
                        },
                        cancellable: true,
                    },
                    deadline_ms: None,
                },
                1_000,
                handle.clone(),
            )
            .unwrap();
        state.take_dialog_props_transitions();

        for current in 1..=7 {
            state
                .dialog_broker
                .update_progress(
                    "musicd",
                    &owner_token,
                    &handle,
                    DialogProgressPatchV1 {
                        message: None,
                        progress: Some(DialogProgressValueV1::Determinate { current, total: 10 }),
                    },
                    1_001,
                )
                .unwrap();
        }
        state.finish_dialog_mutation(1_001);

        let events = crate::drain_props_events(&mut state);
        assert_eq!(events.len(), 1);
        let crate::PropsEvent::Resync { snapshot, .. } = &events[0] else {
            panic!("overflow must publish a full snapshot resync");
        };
        assert_eq!(snapshot["dialogs"]["doverflow"]["progress_fraction"], 0.7);
    }
}
