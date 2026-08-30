use std::time::{Duration, Instant};

use crate::types::{
    AcceptedContext, ActionMask, DataTransferId, Deadline, DndAction, DndOrigin, DragPayload,
    DropComplete, DropDecision, DropDecisionKind, DropEvent, DropOutcome, PayloadFailure,
    ProposalRevision, TargetId, TerminalDisposition, TerminalEvent, TerminalReason,
    TransportRevision,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReceivePhase {
    Offered,
    Fetching,
    Ready,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AskPhase {
    NotAsk,
    AwaitingDecision,
    AskResolved {
        action: DndAction,
    },
    AwaitingCompletion {
        action: DndAction,
        action_acknowledged: bool,
        app_completed: bool,
    },
    Finished,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResourceState {
    pub offer: bool,
    pub source: bool,
    pub custom_pointer: bool,
    pub highlight: bool,
    pub fd_or_task: bool,
    pub active_transfer: bool,
}

impl ResourceState {
    fn receive_active() -> Self {
        Self {
            offer: true,
            source: false,
            custom_pointer: true,
            highlight: true,
            fd_or_task: false,
            active_transfer: true,
        }
    }

    fn clear_all(&mut self) {
        *self = Self {
            offer: false,
            source: false,
            custom_pointer: false,
            highlight: false,
            fd_or_task: false,
            active_transfer: false,
        };
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReceiveEffect {
    EmitDrop(DropEvent),
    SetActions {
        allowed: ActionMask,
        preferred: DndAction,
    },
    HoverCleared {
        transfer_id: DataTransferId,
        post_drop: bool,
    },
    FinishOffer,
    DestroyOffer,
    Terminal(TerminalEvent),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReceiveError {
    StaleTransfer {
        expected: DataTransferId,
        received: DataTransferId,
    },
    OriginMismatch,
    AlreadyTerminal,
    InvalidTransition,
    DeliveryMismatch,
    AskNotPending,
    FinalActionsNotSent,
}

/// Acceptance paired with the transport revision the consumer had observed.
///
/// The pairing is what makes the drop fence provable: an acceptance is only
/// current with respect to revisions at or below `observed`.
#[derive(Clone, Debug)]
struct AcceptedState {
    context: AcceptedContext,
    observed: TransportRevision,
}

/// A physical `wl_data_device.drop` awaiting proof that acceptance is current.
#[derive(Clone, Copy, Debug)]
struct PendingDrop {
    at_revision: TransportRevision,
    deadline: Deadline,
    attempted: bool,
}

#[derive(Clone, Debug)]
struct AwaitingCompletion {
    action: DndAction,
    barrier_id: u64,
    barrier_completed: bool,
    action_acknowledged: bool,
    app_completed: bool,
    /// The final `set_actions` went directly through the retained post-drop
    /// offer because SCTK's wrapper suppresses requests after pointer leave.
    post_leave_path: bool,
}

/// Pure receive-side transfer machine.
///
/// Physical drop and payload readiness are deliberately independent. The
/// complete accepted context is cloned into `drop_snapshot`; no later state is
/// consulted when the canonical drop is emitted.
///
/// # The drop fence
///
/// A compositor may dispatch `motion`, `wl_data_offer.action` and `drop` inside
/// a single `wl_display` pump. The consumer only sees the motion and action
/// after that pump completes, so an acceptance built before the pump describes
/// a target and an action the pointer has already left behind. Snapshotting the
/// drop against it copies into the wrong directory with the wrong operation.
///
/// So a physical drop does not snapshot. [`ReceiveTransfer::physical_drop`]
/// records the transport revision current at the drop callback, and
/// [`ReceiveTransfer::resolve_drop_fence`] snapshots only once acceptance is
/// proven to cover that revision. When no motion or action preceded the drop in
/// the same pump the acceptance already covers it and the drop resolves in the
/// same frame — the one-frame latency is paid only when the race is real. A
/// consumer that never refreshes cannot force a stale delivery: the fence
/// expires after a bounded wall-clock interval and fails closed with
/// [`TerminalReason::DropFenceExpired`].
///
/// Once a physical drop exists, every live state has a bounded way forward:
/// the drop fence, the payload-request deadline, the payload worker's
/// inactivity deadline, or one of the confirmation/completion deadlines.
#[derive(Clone, Debug)]
pub struct ReceiveTransfer {
    id: DataTransferId,
    phase: ReceivePhase,
    accepted: Option<AcceptedState>,
    pending_drop: Option<PendingDrop>,
    drop_snapshot: Option<AcceptedContext>,
    payload: Option<DragPayload>,
    drop_emitted: bool,
    compositor_action: Option<DndAction>,
    ask_phase: AskPhase,
    ask_confirmation_after: Duration,
    post_decision_after: Duration,
    payload_request_deadline: Option<Deadline>,
    ask_confirmation_deadline: Option<Deadline>,
    post_decision_deadline: Option<Deadline>,
    completion_flush_deadline: Option<(Deadline, TerminalReason)>,
    completion_action: Option<DndAction>,
    awaiting_completion: Option<AwaitingCompletion>,
    resources: ResourceState,
    terminal: Option<TerminalEvent>,
    terminal_transition_count: usize,
}

impl ReceiveTransfer {
    pub fn new(
        id: DataTransferId,
        ask_confirmation_after: Duration,
        post_decision_after: Duration,
    ) -> Self {
        Self {
            id,
            ask_confirmation_after,
            post_decision_after,
            phase: ReceivePhase::Offered,
            accepted: None,
            pending_drop: None,
            drop_snapshot: None,
            payload: None,
            drop_emitted: false,
            compositor_action: None,
            ask_phase: AskPhase::NotAsk,
            payload_request_deadline: None,
            ask_confirmation_deadline: None,
            post_decision_deadline: None,
            completion_flush_deadline: None,
            completion_action: None,
            awaiting_completion: None,
            resources: ResourceState::receive_active(),
            terminal: None,
            terminal_transition_count: 0,
        }
    }

    pub fn id(&self) -> DataTransferId {
        self.id
    }

    pub fn phase(&self) -> ReceivePhase {
        self.phase
    }

    /// True once the compositor has dropped, whether or not the fence has
    /// resolved. `leave` after this point is the ordinary post-drop leave.
    pub fn dropped(&self) -> bool {
        self.drop_snapshot.is_some() || self.pending_drop.is_some()
    }

    pub fn drop_pending(&self) -> bool {
        self.dropped() && !self.drop_emitted
    }

    pub fn fence_pending(&self) -> bool {
        self.pending_drop.is_some()
    }

    /// Whether the state machine itself currently owns a terminal deadline.
    ///
    /// While payload I/O is in progress the worker's inactivity deadline is the
    /// owner instead, so `Fetching` intentionally returns true here.
    pub fn has_armed_deadline(&self) -> bool {
        self.pending_drop.is_some()
            || self.payload_request_deadline.is_some()
            || self.ask_confirmation_deadline.is_some()
            || self.post_decision_deadline.is_some()
            || self.completion_flush_deadline.is_some()
            || self.phase == ReceivePhase::Fetching
    }

    pub fn ask_phase(&self) -> AskPhase {
        if let Some(awaiting) = &self.awaiting_completion {
            return AskPhase::AwaitingCompletion {
                action: awaiting.action,
                action_acknowledged: awaiting.action_acknowledged,
                app_completed: awaiting.app_completed,
            };
        }
        self.ask_phase
    }

    pub fn resources(&self) -> ResourceState {
        self.resources
    }

    pub fn terminal_event(&self) -> Option<TerminalEvent> {
        self.terminal
    }

    pub fn terminal_transition_count(&self) -> usize {
        self.terminal_transition_count
    }

    pub(crate) fn completion_flush_deadline(&self) -> Option<(Deadline, TerminalReason)> {
        self.completion_flush_deadline
    }

    pub(crate) fn completion_action(&self) -> Option<DndAction> {
        self.completion_action
    }

    /// Applies a target acceptance together with the newest transport revision
    /// the consumer has actually observed.
    pub fn accept(
        &mut self,
        context: AcceptedContext,
        observed: TransportRevision,
    ) -> Result<(), ReceiveError> {
        self.accept_for_origin(context, observed, DndOrigin::External(self.id))
    }

    pub(crate) fn accept_for_origin(
        &mut self,
        context: AcceptedContext,
        observed: TransportRevision,
        expected_origin: DndOrigin,
    ) -> Result<(), ReceiveError> {
        self.ensure_live()?;
        // Acceptance stays mutable while the fence is unresolved — refreshing it
        // is precisely how the consumer proves currency. Once snapshotted the
        // context is frozen for the life of the delivery.
        if self.drop_snapshot.is_some() {
            return Err(ReceiveError::InvalidTransition);
        }
        if context.origin != expected_origin {
            return match context.origin {
                DndOrigin::External(received) => Err(ReceiveError::StaleTransfer {
                    expected: self.id,
                    received,
                }),
                DndOrigin::Internal(_) => Err(ReceiveError::OriginMismatch),
            };
        }
        self.accepted = Some(AcceptedState { context, observed });
        Ok(())
    }

    pub fn begin_fetch(&mut self, now: Instant) -> Result<Vec<ReceiveEffect>, ReceiveError> {
        self.ensure_live()?;
        if let Some(effects) = self.expire_armed_deadline(now) {
            return Ok(effects);
        }
        if self.accepted.is_none() || self.phase != ReceivePhase::Offered {
            return Err(ReceiveError::InvalidTransition);
        }
        self.phase = ReceivePhase::Fetching;
        self.payload_request_deadline = None;
        self.resources.fd_or_task = true;
        Ok(Vec::new())
    }

    pub fn clear_acceptance(&mut self) -> Result<(), ReceiveError> {
        self.ensure_live()?;
        if self.drop_snapshot.is_some() {
            return Err(ReceiveError::InvalidTransition);
        }
        self.accepted = None;
        Ok(())
    }

    /// Records the compositor's physical drop against the transport revision
    /// current at that callback. Nothing is snapshotted here; see the type docs.
    pub fn physical_drop(
        &mut self,
        at_revision: TransportRevision,
        now: Instant,
        fence_timeout: Duration,
    ) -> Result<(), ReceiveError> {
        self.ensure_live()?;
        if self.dropped() {
            return Err(ReceiveError::InvalidTransition);
        }
        self.pending_drop = Some(PendingDrop {
            at_revision,
            deadline: deadline_after(now, fence_timeout),
            attempted: false,
        });
        if self.phase == ReceivePhase::Offered {
            self.payload_request_deadline = Some(deadline_after(now, self.ask_confirmation_after));
        }
        Ok(())
    }

    /// Resolves the drop fence at the end of a pump.
    ///
    /// Snapshots when the current acceptance covers the drop's revision;
    /// otherwise gives the consumer at least one resolution opportunity and
    /// fails closed once the wall-clock deadline has elapsed.
    pub fn resolve_drop_fence(&mut self, now: Instant) -> Result<Vec<ReceiveEffect>, ReceiveError> {
        self.ensure_live()?;
        let Some(pending) = self.pending_drop else {
            return Ok(Vec::new());
        };
        if pending.attempted && now >= pending.deadline.at {
            self.pending_drop = None;
            return Ok(self.terminate(
                TerminalDisposition::Rejected,
                TerminalReason::DropFenceExpired,
            ));
        }
        let covered = self
            .accepted
            .as_ref()
            .is_some_and(|accepted| accepted.observed >= pending.at_revision);
        if !covered {
            self.pending_drop = Some(PendingDrop {
                attempted: true,
                ..pending
            });
            return Ok(Vec::new());
        }

        self.pending_drop = None;
        let Some(context) = self
            .accepted
            .as_ref()
            .map(|accepted| accepted.context.clone())
        else {
            return Ok(self.terminate(
                TerminalDisposition::Rejected,
                TerminalReason::DropFenceExpired,
            ));
        };
        self.drop_snapshot = Some(context);
        Ok(self.maybe_emit_drop(now))
    }

    pub fn payload_ready(
        &mut self,
        transfer_id: DataTransferId,
        result: Result<DragPayload, PayloadFailure>,
        now: Instant,
    ) -> Result<Vec<ReceiveEffect>, ReceiveError> {
        self.ensure_id(transfer_id)?;
        if self.terminal.is_some() {
            return Ok(Vec::new());
        }
        if let Some(effects) = self.expire_armed_deadline(now) {
            return Ok(effects);
        }
        if self.phase != ReceivePhase::Fetching {
            return Ok(self.terminate(
                TerminalDisposition::Rejected,
                TerminalReason::LateWorkerResult,
            ));
        }
        self.resources.fd_or_task = false;
        match result {
            Ok(payload) => {
                self.payload = Some(payload);
                self.phase = ReceivePhase::Ready;
                Ok(self.maybe_emit_drop(now))
            }
            Err(failure) => Ok(self.terminate(TerminalDisposition::Rejected, failure.reason())),
        }
    }

    pub fn invalidate_revision(
        &mut self,
        revision: ProposalRevision,
        now: Instant,
    ) -> Result<Vec<ReceiveEffect>, ReceiveError> {
        self.ensure_live()?;
        if let Some(effects) = self.expire_armed_deadline(now) {
            return Ok(effects);
        }
        let affected = self
            .drop_snapshot
            .as_ref()
            .or(self.accepted.as_ref().map(|accepted| &accepted.context))
            .is_some_and(|context| context.revision == revision);
        if !affected {
            return Ok(Vec::new());
        }
        Ok(self.terminate(
            TerminalDisposition::Rejected,
            TerminalReason::RevisionInvalidated,
        ))
    }

    pub fn target_lost(
        &mut self,
        target: TargetId,
        now: Instant,
    ) -> Result<Vec<ReceiveEffect>, ReceiveError> {
        self.ensure_live()?;
        if let Some(effects) = self.expire_armed_deadline(now) {
            return Ok(effects);
        }
        let affected = self
            .drop_snapshot
            .as_ref()
            .or(self.accepted.as_ref().map(|accepted| &accepted.context))
            .is_some_and(|context| context.target == target);
        if !affected {
            return Ok(Vec::new());
        }
        Ok(self.terminate(TerminalDisposition::Rejected, TerminalReason::TargetLost))
    }

    pub fn leave(&mut self, now: Instant) -> Result<Vec<ReceiveEffect>, ReceiveError> {
        self.ensure_live()?;
        if let Some(effects) = self.expire_armed_deadline(now) {
            return Ok(effects);
        }
        self.resources.custom_pointer = false;
        self.resources.highlight = false;
        if self.dropped() {
            Ok(vec![ReceiveEffect::HoverCleared {
                transfer_id: self.id,
                post_drop: true,
            }])
        } else {
            let mut effects = vec![ReceiveEffect::HoverCleared {
                transfer_id: self.id,
                post_drop: false,
            }];
            effects.extend(self.terminate(
                TerminalDisposition::Rejected,
                TerminalReason::LeaveBeforeDrop,
            ));
            Ok(effects)
        }
    }

    pub fn drop_decision(
        &mut self,
        decision: DropDecision,
        now: Instant,
        post_decision_after: Duration,
    ) -> Result<Vec<ReceiveEffect>, ReceiveError> {
        self.ensure_live()?;
        if let Some(effects) = self.expire_armed_deadline(now) {
            return Ok(effects);
        }
        let snapshot = self
            .drop_snapshot
            .as_ref()
            .ok_or(ReceiveError::AskNotPending)?;
        if snapshot.delivery_id != decision.delivery_id {
            return Err(ReceiveError::DeliveryMismatch);
        }
        if self.ask_phase != AskPhase::AwaitingDecision {
            return Err(ReceiveError::AskNotPending);
        }

        let action = match decision.decision {
            DropDecisionKind::Copy => DndAction::Copy,
            DropDecisionKind::Move => DndAction::Move,
            DropDecisionKind::Dismissed => {
                return Ok(
                    self.terminate(TerminalDisposition::Rejected, TerminalReason::AppDismissed)
                );
            }
        };
        self.ask_phase = AskPhase::AskResolved { action };
        self.ask_confirmation_deadline = None;
        self.post_decision_deadline = Some(deadline_after(now, post_decision_after));
        let allowed = match action {
            DndAction::Copy => ActionMask::COPY,
            DndAction::Move => ActionMask::MOVE,
            DndAction::Ask => unreachable!("Ask cannot resolve to Ask"),
        };
        Ok(vec![ReceiveEffect::SetActions {
            allowed,
            preferred: action,
        }])
    }

    /// Marks the final non-Ask `set_actions` request as sent.
    ///
    /// The named `wl_display.sync` closes the causal window. When its `done`
    /// arrives, the latest compositor action ordered before it is checked
    /// against this latch.
    /// `post_leave_path` records that the request used the retained offer
    /// directly because SCTK suppresses `set_actions` after pointer leave.
    pub fn final_actions_sent(
        &mut self,
        barrier_id: u64,
        post_leave_path: bool,
        now: Instant,
    ) -> Result<Vec<ReceiveEffect>, ReceiveError> {
        self.ensure_live()?;
        if let Some(effects) = self.expire_armed_deadline(now) {
            return Ok(effects);
        }
        let AskPhase::AskResolved { action } = self.ask_phase else {
            return Err(ReceiveError::FinalActionsNotSent);
        };
        self.post_decision_deadline
            .ok_or(ReceiveError::FinalActionsNotSent)?;
        self.awaiting_completion = Some(AwaitingCompletion {
            action,
            barrier_id,
            barrier_completed: false,
            action_acknowledged: false,
            app_completed: false,
            post_leave_path,
        });
        Ok(Vec::new())
    }

    /// Consumes a compositor action callback.
    ///
    /// `None` means the compositor currently matched no action. It is recorded
    /// even outside Ask completion so ordinary drops can be validated.
    pub fn compositor_action(
        &mut self,
        action: Option<DndAction>,
        now: Instant,
    ) -> Result<Vec<ReceiveEffect>, ReceiveError> {
        self.ensure_live()?;
        if let Some(effects) = self.expire_armed_deadline(now) {
            return Ok(effects);
        }
        self.compositor_action = action;
        if !self.drop_emitted
            && let Some(action) = action.filter(|action| *action != DndAction::Ask)
        {
            if let Some(snapshot) = self
                .drop_snapshot
                .as_mut()
                .filter(|snapshot| snapshot.action != DndAction::Ask)
            {
                snapshot.action = action;
            } else if let Some(accepted) = self
                .accepted
                .as_mut()
                .filter(|accepted| accepted.context.action != DndAction::Ask)
            {
                accepted.context.action = action;
            }
        }
        let Some(awaiting) = self.awaiting_completion.as_mut() else {
            return Ok(Vec::new());
        };
        if !awaiting.barrier_completed {
            return Ok(Vec::new());
        }
        let Some(action) = action else {
            return Ok(self.terminate(
                TerminalDisposition::Rejected,
                TerminalReason::FinalActionRejected,
            ));
        };
        if action == DndAction::Ask || action != awaiting.action {
            awaiting.action_acknowledged = false;
            return Ok(Vec::new());
        }
        awaiting.action_acknowledged = true;
        awaiting.post_leave_path = false;
        Ok(Vec::new())
    }

    /// Completes the post-`set_actions` sync barrier with the compositor action
    /// state ordered before its `done`.
    pub fn final_action_barrier_done(
        &mut self,
        barrier_id: u64,
        action: Option<DndAction>,
        now: Instant,
    ) -> Result<Vec<ReceiveEffect>, ReceiveError> {
        self.ensure_live()?;
        if let Some(effects) = self.expire_armed_deadline(now) {
            return Ok(effects);
        }
        self.compositor_action = action;
        let Some(awaiting) = self.awaiting_completion.as_mut() else {
            return Ok(Vec::new());
        };
        if barrier_id != awaiting.barrier_id {
            return Ok(Vec::new());
        }
        awaiting.barrier_completed = true;
        let Some(action) = action else {
            return Ok(self.terminate(
                TerminalDisposition::Rejected,
                TerminalReason::FinalActionRejected,
            ));
        };
        if action == DndAction::Ask || action != awaiting.action {
            return Ok(Vec::new());
        }
        awaiting.action_acknowledged = true;
        awaiting.post_leave_path = false;
        Ok(Vec::new())
    }

    /// Settles a fully observed Ask latch after the current protocol batch.
    pub(crate) fn settle_completion(
        &mut self,
        now: Instant,
    ) -> Result<Vec<ReceiveEffect>, ReceiveError> {
        self.ensure_live()?;
        if let Some(effects) = self.expire_armed_deadline(now) {
            return Ok(effects);
        }
        Ok(self.maybe_finish_ask())
    }

    pub fn drop_complete(
        &mut self,
        complete: DropComplete,
        now: Instant,
    ) -> Result<Vec<ReceiveEffect>, ReceiveError> {
        self.ensure_live()?;
        if let Some(effects) = self.expire_armed_deadline(now) {
            return Ok(effects);
        }
        let snapshot = self
            .drop_snapshot
            .as_ref()
            .ok_or(ReceiveError::InvalidTransition)?;
        if !self.drop_emitted {
            return Err(ReceiveError::InvalidTransition);
        }
        if snapshot.delivery_id != complete.delivery_id {
            return Err(ReceiveError::DeliveryMismatch);
        }
        match complete.outcome {
            DropOutcome::Failed => Ok(self.terminate(
                TerminalDisposition::Rejected,
                TerminalReason::AppOperationFailed,
            )),
            DropOutcome::Completed(action) => {
                if snapshot.action != DndAction::Ask {
                    if action != snapshot.action {
                        return Ok(self.terminate(
                            TerminalDisposition::Rejected,
                            TerminalReason::ActionMismatch,
                        ));
                    }
                    self.completion_action = Some(action);
                    return Ok(
                        self.terminate(TerminalDisposition::Finished, TerminalReason::Completed)
                    );
                }

                let Some(awaiting) = self.awaiting_completion.as_mut() else {
                    return Err(ReceiveError::FinalActionsNotSent);
                };
                if action != awaiting.action {
                    return Ok(self.terminate(
                        TerminalDisposition::Rejected,
                        TerminalReason::ActionMismatch,
                    ));
                }
                awaiting.app_completed = true;
                Ok(self.maybe_finish_ask())
            }
        }
    }

    /// Checks every state-machine-owned deadline.
    pub fn check_deadline(&mut self, now: Instant) -> Result<Vec<ReceiveEffect>, ReceiveError> {
        self.ensure_live()?;
        Ok(self.expire_armed_deadline(now).unwrap_or_default())
    }

    /// Drives every failure source through the same idempotent cleanup path.
    pub fn fail(
        &mut self,
        transfer_id: DataTransferId,
        reason: TerminalReason,
        now: Instant,
    ) -> Result<Vec<ReceiveEffect>, ReceiveError> {
        self.ensure_id(transfer_id)?;
        if self.terminal.is_some() {
            return Ok(Vec::new());
        }
        if let Some(effects) = self.expire_armed_deadline(now) {
            return Ok(effects);
        }
        Ok(self.terminate(TerminalDisposition::Rejected, reason))
    }

    /// Records an offer replacement observed by the compositor callback.
    ///
    /// Replacement wins over a deadline whose expiry has not yet been observed
    /// by a bridge pump: SCTK has already destroyed the old proxy, so
    /// `OfferReplaced` is the concrete cause and no request can recover it.
    pub(crate) fn offer_replaced(
        &mut self,
        transfer_id: DataTransferId,
    ) -> Result<Vec<ReceiveEffect>, ReceiveError> {
        self.ensure_id(transfer_id)?;
        if self.terminal.is_some() {
            return Ok(Vec::new());
        }
        Ok(self.terminate(TerminalDisposition::Rejected, TerminalReason::OfferReplaced))
    }

    fn maybe_emit_drop(&mut self, now: Instant) -> Vec<ReceiveEffect> {
        if self.drop_emitted || self.phase != ReceivePhase::Ready {
            return Vec::new();
        }
        let Some(snapshot) = self.drop_snapshot.clone() else {
            return Vec::new();
        };
        let Some(payload) = self.payload.clone() else {
            return Vec::new();
        };

        if snapshot.action != DndAction::Ask && self.compositor_action != Some(snapshot.action) {
            return self.terminate(
                TerminalDisposition::Rejected,
                TerminalReason::ActionMismatch,
            );
        }

        self.drop_emitted = true;
        self.payload_request_deadline = None;
        self.ask_phase = if snapshot.action == DndAction::Ask {
            // The confirmation wait starts here, not at resolution: an Ask drop
            // whose confirm is abandoned or whose owner dies must still reach a
            // terminal instead of holding the offer forever.
            self.ask_confirmation_deadline = Some(deadline_after(now, self.ask_confirmation_after));
            AskPhase::AwaitingDecision
        } else {
            self.post_decision_deadline = Some(deadline_after(now, self.post_decision_after));
            AskPhase::NotAsk
        };
        vec![ReceiveEffect::EmitDrop(DropEvent {
            transfer_id: self.id,
            target: snapshot.target,
            payload,
            action: snapshot.action,
            modifiers: snapshot.modifiers,
            origin: snapshot.origin,
            delivery_id: snapshot.delivery_id,
            accepted_revision: snapshot.revision,
        })]
    }

    fn maybe_finish_ask(&mut self) -> Vec<ReceiveEffect> {
        if self
            .awaiting_completion
            .as_ref()
            .is_some_and(|awaiting| awaiting.action_acknowledged && awaiting.app_completed)
        {
            self.completion_action = self
                .awaiting_completion
                .as_ref()
                .map(|awaiting| awaiting.action);
            self.ask_phase = AskPhase::Finished;
            return self.terminate(TerminalDisposition::Finished, TerminalReason::Completed);
        }
        Vec::new()
    }

    /// Expires the currently armed state-machine deadline before a late event
    /// can clear, replace, or satisfy it.
    fn expire_armed_deadline(&mut self, now: Instant) -> Option<Vec<ReceiveEffect>> {
        let reason = if self
            .payload_request_deadline
            .is_some_and(|deadline| now >= deadline.at)
        {
            Some(TerminalReason::PayloadRequestDeadlineExpired)
        } else if self
            .ask_confirmation_deadline
            .is_some_and(|deadline| now >= deadline.at)
        {
            Some(TerminalReason::AskConfirmationDeadlineExpired)
        } else if self
            .post_decision_deadline
            .is_some_and(|deadline| now >= deadline.at)
        {
            Some(
                if self.awaiting_completion.as_ref().is_some_and(|awaiting| {
                    awaiting.post_leave_path && !awaiting.action_acknowledged
                }) {
                    TerminalReason::PostDropFinalActionDeadlineExpired
                } else {
                    TerminalReason::PostDecisionDeadlineExpired
                },
            )
        } else {
            None
        }?;
        Some(self.terminate(TerminalDisposition::Rejected, reason))
    }

    fn terminate(
        &mut self,
        disposition: TerminalDisposition,
        reason: TerminalReason,
    ) -> Vec<ReceiveEffect> {
        if self.terminal.is_some() {
            return Vec::new();
        }
        if disposition == TerminalDisposition::Finished {
            let expiry_reason =
                if self.awaiting_completion.as_ref().is_some_and(|awaiting| {
                    awaiting.post_leave_path && !awaiting.action_acknowledged
                }) {
                    TerminalReason::PostDropFinalActionDeadlineExpired
                } else {
                    TerminalReason::PostDecisionDeadlineExpired
                };
            self.completion_flush_deadline = self
                .post_decision_deadline
                .map(|deadline| (deadline, expiry_reason));
        }
        self.resources.clear_all();
        self.awaiting_completion = None;
        self.payload_request_deadline = None;
        self.ask_confirmation_deadline = None;
        self.post_decision_deadline = None;
        self.pending_drop = None;
        self.terminal_transition_count += 1;
        let event = TerminalEvent {
            transfer_id: self.id,
            disposition,
            reason,
        };
        self.terminal = Some(event);

        let offer_effect = match disposition {
            TerminalDisposition::Finished => ReceiveEffect::FinishOffer,
            TerminalDisposition::Rejected => ReceiveEffect::DestroyOffer,
        };
        vec![offer_effect, ReceiveEffect::Terminal(event)]
    }

    fn ensure_id(&self, received: DataTransferId) -> Result<(), ReceiveError> {
        if self.id != received {
            return Err(ReceiveError::StaleTransfer {
                expected: self.id,
                received,
            });
        }
        Ok(())
    }

    fn ensure_live(&self) -> Result<(), ReceiveError> {
        if self.terminal.is_some() {
            Err(ReceiveError::AlreadyTerminal)
        } else {
            Ok(())
        }
    }
}

fn deadline_after(now: Instant, duration: Duration) -> Deadline {
    Deadline {
        at: now
            .checked_add(duration)
            .unwrap_or_else(|| furthest_representable_instant(now, duration)),
    }
}

fn furthest_representable_instant(now: Instant, upper_bound: Duration) -> Instant {
    let mut valid_nanos = 0_u128;
    let mut invalid_nanos = upper_bound.as_nanos();
    while valid_nanos + 1 < invalid_nanos {
        let candidate_nanos = valid_nanos + (invalid_nanos - valid_nanos) / 2;
        let candidate = duration_from_nanos(candidate_nanos);
        if now.checked_add(candidate).is_some() {
            valid_nanos = candidate_nanos;
        } else {
            invalid_nanos = candidate_nanos;
        }
    }
    now.checked_add(duration_from_nanos(valid_nanos))
        .expect("zero duration is always representable")
}

fn duration_from_nanos(nanos: u128) -> Duration {
    Duration::new(
        (nanos / 1_000_000_000) as u64,
        (nanos % 1_000_000_000) as u32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{DeliveryId, Modifiers, PayloadFailure, SourceId};
    use std::path::PathBuf;

    const ID: DataTransferId = DataTransferId(1);
    const CONFIRM: Duration = Duration::from_secs(30);
    const POST_DECISION: Duration = Duration::from_secs(300);
    const FENCE: Duration = Duration::from_millis(500);

    fn context(target: u64, action: DndAction) -> AcceptedContext {
        AcceptedContext {
            target: TargetId(target),
            action,
            modifiers: Modifiers::default(),
            origin: DndOrigin::External(ID),
            delivery_id: DeliveryId(9),
            revision: ProposalRevision(1),
        }
    }

    fn payload() -> DragPayload {
        DragPayload::Paths(vec![PathBuf::from("/tmp/a")])
    }

    fn transfer() -> ReceiveTransfer {
        ReceiveTransfer::new(ID, CONFIRM, POST_DECISION)
    }

    /// Accepted, fetching, with a payload already in hand — the state a real
    /// transfer is in by the time the user releases the button.
    fn ready(target: u64, action: DndAction, revision: u64, now: Instant) -> ReceiveTransfer {
        let mut transfer = transfer();
        transfer
            .accept(context(target, action), TransportRevision(revision))
            .unwrap();
        assert!(transfer.begin_fetch(now).unwrap().is_empty());
        transfer
            .payload_ready(ID, Ok(payload()), now)
            .expect("payload accepted");
        transfer
            .compositor_action(Some(action), now)
            .expect("compositor action accepted");
        transfer
    }

    fn dropped_event(effects: &[ReceiveEffect]) -> Option<DropEvent> {
        effects.iter().find_map(|effect| match effect {
            ReceiveEffect::EmitDrop(event) => Some(event.clone()),
            _ => None,
        })
    }

    fn terminal_reason(effects: &[ReceiveEffect]) -> Option<TerminalReason> {
        effects.iter().find_map(|effect| match effect {
            ReceiveEffect::Terminal(event) => Some(event.reason),
            _ => None,
        })
    }

    // ---- the drop fence -------------------------------------------------

    /// The BLOCKER, reproduced at the seam it lives on: KWin dispatches
    /// motion(pane B), action(Move) and drop inside one pump. Acceptance still
    /// says pane A + Copy when the drop is recorded, and the consumer only
    /// learns about pane B after that pump. The drop must land on B + Move.
    #[test]
    fn a_same_pump_motion_and_action_do_not_deliver_the_previous_target() {
        // KWin trace: motion, wl_data_offer.action and wl_data_device.drop can
        // all be dispatched before one winit AboutToWait pump.
        let now = Instant::now();
        let mut transfer = ready(1, DndAction::Copy, 1, now);

        // --- pump N: motion(rev 2), action(rev 3) and drop all arrive.
        transfer
            .physical_drop(TransportRevision(3), now, FENCE)
            .unwrap();
        let effects = transfer.resolve_drop_fence(now).unwrap();
        assert!(
            dropped_event(&effects).is_none(),
            "the drop must not resolve against pane A + Copy"
        );
        assert!(transfer.fence_pending());

        // --- between pumps: the consumer sees motion+action, re-hit-tests to
        //     pane B, and re-accepts covering revision 3.
        transfer
            .accept(context(2, DndAction::Move), TransportRevision(3))
            .unwrap();
        transfer
            .compositor_action(Some(DndAction::Move), now)
            .unwrap();

        // --- pump N+1: acceptance now covers the drop.
        let effects = transfer.resolve_drop_fence(now).unwrap();
        let event = dropped_event(&effects).expect("drop delivered once proven current");
        assert_eq!(event.target, TargetId(2));
        assert_eq!(event.action, DndAction::Move);
    }

    /// The common case pays no latency: nothing preceded the drop in its pump,
    /// so the acceptance the consumer already holds covers it immediately.
    #[test]
    fn a_drop_with_no_preceding_callbacks_resolves_in_the_same_frame() {
        let now = Instant::now();
        let mut transfer = ready(1, DndAction::Copy, 7, now);

        transfer
            .physical_drop(TransportRevision(7), now, FENCE)
            .unwrap();
        let event = dropped_event(&transfer.resolve_drop_fence(now).unwrap())
            .expect("no stale callbacks, so no wait");
        assert_eq!(event.target, TargetId(1));
        assert_eq!(event.action, DndAction::Copy);
    }

    /// A consumer that never refreshes acceptance cannot force a stale
    /// delivery, and cannot wedge the transfer either: the fence is bounded.
    #[test]
    fn a_consumer_that_never_refreshes_expires_the_fence_instead_of_delivering() {
        let now = Instant::now();
        let mut transfer = ready(1, DndAction::Copy, 1, now);
        transfer
            .physical_drop(TransportRevision(9), now, FENCE)
            .unwrap();

        let first = transfer.resolve_drop_fence(now + FENCE).unwrap();
        assert!(first.is_empty(), "an overdue fence still gets one attempt");
        let effects = transfer.resolve_drop_fence(now + FENCE).unwrap();
        assert!(dropped_event(&effects).is_none());
        assert_eq!(
            terminal_reason(&effects),
            Some(TerminalReason::DropFenceExpired)
        );
        assert!(matches!(effects[0], ReceiveEffect::DestroyOffer));
    }

    #[test]
    fn overdue_fence_cannot_be_revived_by_accepting_after_the_first_attempt() {
        let now = Instant::now();
        let mut transfer = ready(1, DndAction::Copy, 1, now);
        transfer
            .physical_drop(TransportRevision(2), now, FENCE)
            .unwrap();

        assert!(transfer.resolve_drop_fence(now).unwrap().is_empty());
        transfer
            .accept(context(2, DndAction::Move), TransportRevision(2))
            .unwrap();
        transfer
            .compositor_action(Some(DndAction::Move), now)
            .unwrap();

        let effects = transfer.resolve_drop_fence(now + FENCE).unwrap();
        assert!(dropped_event(&effects).is_none());
        assert_eq!(
            terminal_reason(&effects),
            Some(TerminalReason::DropFenceExpired)
        );
    }

    /// An acceptance from *before* the drop's revision is not proof, even
    /// though it arrived after the drop was recorded.
    #[test]
    fn a_stale_revision_does_not_satisfy_the_fence() {
        let now = Instant::now();
        let mut transfer = ready(1, DndAction::Copy, 1, now);
        transfer
            .physical_drop(TransportRevision(5), now, FENCE)
            .unwrap();
        transfer
            .accept(context(2, DndAction::Move), TransportRevision(4))
            .unwrap();

        assert!(dropped_event(&transfer.resolve_drop_fence(now).unwrap()).is_none());
    }

    #[test]
    fn acceptance_is_frozen_once_the_snapshot_is_taken() {
        let now = Instant::now();
        let mut transfer = ready(1, DndAction::Copy, 1, now);
        transfer
            .physical_drop(TransportRevision(1), now, FENCE)
            .unwrap();
        transfer.resolve_drop_fence(now).unwrap();

        assert_eq!(
            transfer.accept(context(2, DndAction::Move), TransportRevision(2)),
            Err(ReceiveError::InvalidTransition)
        );
        assert_eq!(
            transfer.clear_acceptance(),
            Err(ReceiveError::InvalidTransition)
        );
    }

    /// A leave arriving while the fence is still unresolved is the ordinary
    /// post-drop leave, not an abandoned drag.
    #[test]
    fn a_leave_during_an_unresolved_fence_is_a_post_drop_leave() {
        // SCTK 0.19.2 data_device.rs:181-197 marks dropped before its later
        // data_device.rs:157-167 Leave callback.
        let now = Instant::now();
        let mut transfer = ready(1, DndAction::Copy, 1, now);
        transfer
            .physical_drop(TransportRevision(9), now, FENCE)
            .unwrap();

        let effects = transfer.leave(now).unwrap();
        assert_eq!(
            effects,
            vec![ReceiveEffect::HoverCleared {
                transfer_id: ID,
                post_drop: true,
            }]
        );
        assert!(transfer.terminal_event().is_none());
    }

    #[test]
    fn a_leave_before_any_drop_terminates_the_transfer() {
        let now = Instant::now();
        let mut transfer = ready(1, DndAction::Copy, 1, now);
        let effects = transfer.leave(now).unwrap();
        assert_eq!(
            terminal_reason(&effects),
            Some(TerminalReason::LeaveBeforeDrop)
        );
    }

    #[test]
    fn a_non_ask_drop_uses_the_compositors_latest_selected_action() {
        // wayland.xml wl_data_device.drop: copy/move uses the last action event
        // received before drop.
        let now = Instant::now();
        let mut transfer = ready(1, DndAction::Move, 1, now);
        transfer
            .compositor_action(Some(DndAction::Copy), now)
            .unwrap();
        transfer
            .physical_drop(TransportRevision(1), now, FENCE)
            .unwrap();

        let effects = transfer.resolve_drop_fence(now).unwrap();
        let event = dropped_event(&effects).expect("selected action is protocol truth");
        assert_eq!(event.action, DndAction::Copy);
    }

    #[test]
    fn a_late_action_cannot_rewrite_the_drop_already_delivered_to_the_app() {
        let now = Instant::now();
        let mut transfer = ready(1, DndAction::Copy, 1, now);
        transfer
            .physical_drop(TransportRevision(1), now, FENCE)
            .unwrap();
        let drop =
            dropped_event(&transfer.resolve_drop_fence(now).unwrap()).expect("copy drop delivered");
        assert_eq!(drop.action, DndAction::Copy);

        transfer
            .compositor_action(Some(DndAction::Move), now)
            .unwrap();
        let effects = transfer
            .drop_complete(
                DropComplete {
                    delivery_id: drop.delivery_id,
                    outcome: DropOutcome::Completed(DndAction::Copy),
                },
                now,
            )
            .unwrap();
        assert_eq!(terminal_reason(&effects), Some(TerminalReason::Completed));
    }

    #[test]
    fn a_non_ask_drop_without_a_compositor_action_fails_closed() {
        // wayland.xml wl_data_offer.action allows none when no action is chosen;
        // wl_data_device.drop supplies no separate copy/move action.
        let now = Instant::now();
        let mut transfer = transfer();
        transfer
            .accept(context(1, DndAction::Copy), TransportRevision(1))
            .unwrap();
        assert!(transfer.begin_fetch(now).unwrap().is_empty());
        transfer.payload_ready(ID, Ok(payload()), now).unwrap();
        transfer
            .physical_drop(TransportRevision(1), now, FENCE)
            .unwrap();

        let effects = transfer.resolve_drop_fence(now).unwrap();
        assert_eq!(
            terminal_reason(&effects),
            Some(TerminalReason::ActionMismatch)
        );
        assert!(dropped_event(&effects).is_none());
    }

    #[test]
    fn a_non_ask_drop_without_drop_complete_expires() {
        let now = Instant::now();
        let mut transfer = ready(1, DndAction::Copy, 1, now);
        transfer
            .physical_drop(TransportRevision(1), now, FENCE)
            .unwrap();
        assert!(dropped_event(&transfer.resolve_drop_fence(now).unwrap()).is_some());

        let effects = transfer.check_deadline(now + POST_DECISION).unwrap();
        assert_eq!(
            terminal_reason(&effects),
            Some(TerminalReason::PostDecisionDeadlineExpired)
        );
    }

    #[test]
    fn an_accepted_drop_without_a_payload_request_expires() {
        let now = Instant::now();
        let mut transfer = transfer();
        transfer
            .accept(context(1, DndAction::Copy), TransportRevision(1))
            .unwrap();
        transfer
            .physical_drop(TransportRevision(1), now, FENCE)
            .unwrap();
        assert!(transfer.resolve_drop_fence(now).unwrap().is_empty());
        assert!(transfer.has_armed_deadline());

        let effects = transfer.check_deadline(now + CONFIRM).unwrap();
        assert_eq!(
            terminal_reason(&effects),
            Some(TerminalReason::PayloadRequestDeadlineExpired)
        );
    }

    #[test]
    fn every_post_drop_live_state_has_a_terminal_deadline_owner() {
        // wayland.xml wl_data_offer.finish keeps the offer live through the
        // final request/flush, so that hidden completion also needs a deadline.
        let now = Instant::now();

        let mut offered = transfer();
        offered
            .accept(context(1, DndAction::Copy), TransportRevision(1))
            .unwrap();
        offered
            .physical_drop(TransportRevision(1), now, FENCE)
            .unwrap();
        assert!(offered.has_armed_deadline());
        offered.resolve_drop_fence(now).unwrap();
        assert!(offered.has_armed_deadline());

        assert!(offered.begin_fetch(now).unwrap().is_empty());
        assert!(
            offered.has_armed_deadline(),
            "Fetching is owned by the worker inactivity deadline"
        );
        offered
            .compositor_action(Some(DndAction::Copy), now)
            .unwrap();
        offered.payload_ready(ID, Ok(payload()), now).unwrap();
        assert!(offered.has_armed_deadline());

        let mut ask = awaiting_decision(now);
        assert!(ask.has_armed_deadline());
        ask.drop_decision(
            DropDecision {
                delivery_id: DeliveryId(9),
                decision: DropDecisionKind::Copy,
            },
            now,
            POST_DECISION,
        )
        .unwrap();
        assert!(ask.has_armed_deadline());
        ask.final_actions_sent(5, true, now).unwrap();
        assert!(ask.has_armed_deadline());
        ask.final_action_barrier_done(5, Some(DndAction::Copy), now)
            .unwrap();
        let effects = ask
            .drop_complete(
                DropComplete {
                    delivery_id: DeliveryId(9),
                    outcome: DropOutcome::Completed(DndAction::Copy),
                },
                now,
            )
            .unwrap();
        assert_eq!(terminal_reason(&effects), Some(TerminalReason::Completed));
        assert!(
            ask.has_armed_deadline(),
            "pending finish flush retains the post-decision deadline"
        );
        assert!(ask.completion_flush_deadline().is_some());
    }

    // ---- Ask deadlines --------------------------------------------------

    fn awaiting_decision(now: Instant) -> ReceiveTransfer {
        let mut transfer = ready(1, DndAction::Ask, 1, now);
        transfer
            .physical_drop(TransportRevision(1), now, FENCE)
            .unwrap();
        let effects = transfer.resolve_drop_fence(now).unwrap();
        assert!(dropped_event(&effects).is_some());
        assert_eq!(transfer.ask_phase(), AskPhase::AwaitingDecision);
        transfer
    }

    /// The confirmation wait exists at all — previously an `Ask` sitting in
    /// `AwaitingDecision` had no deadline and pumping forever did nothing.
    #[test]
    fn an_abandoned_ask_confirmation_fails_closed() {
        let now = Instant::now();
        let mut transfer = awaiting_decision(now);

        assert!(
            transfer
                .check_deadline(now + CONFIRM / 2)
                .unwrap()
                .is_empty()
        );

        let effects = transfer.check_deadline(now + CONFIRM).unwrap();
        assert_eq!(
            terminal_reason(&effects),
            Some(TerminalReason::AskConfirmationDeadlineExpired)
        );
        assert!(matches!(effects[0], ReceiveEffect::DestroyOffer));
        assert_eq!(transfer.terminal_transition_count(), 1);

        // Idempotent: later pumps produce nothing more.
        assert_eq!(
            transfer.check_deadline(now + CONFIRM * 10),
            Err(ReceiveError::AlreadyTerminal)
        );
        assert_eq!(transfer.terminal_transition_count(), 1);
    }

    #[test]
    fn resolving_an_ask_disarms_the_confirmation_wait_and_arms_the_post_decision_wait() {
        let now = Instant::now();
        let mut transfer = awaiting_decision(now);
        transfer
            .drop_decision(
                DropDecision {
                    delivery_id: DeliveryId(9),
                    decision: DropDecisionKind::Move,
                },
                now,
                POST_DECISION,
            )
            .unwrap();
        assert!(
            transfer
                .final_actions_sent(5, true, now)
                .unwrap()
                .is_empty()
        );

        // Well past the confirmation wait, but inside the post-decision one:
        // the application's file operation is still allowed to be running.
        assert!(
            transfer
                .check_deadline(now + CONFIRM * 2)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn an_ask_decision_submitted_after_confirmation_expiry_cannot_rearm_the_deadline() {
        let now = Instant::now();
        let mut transfer = awaiting_decision(now);

        let effects = transfer
            .drop_decision(
                DropDecision {
                    delivery_id: DeliveryId(9),
                    decision: DropDecisionKind::Move,
                },
                now + CONFIRM,
                POST_DECISION,
            )
            .unwrap();

        assert_eq!(
            terminal_reason(&effects),
            Some(TerminalReason::AskConfirmationDeadlineExpired)
        );
        assert_eq!(transfer.terminal_transition_count(), 1);
    }

    /// An unacknowledged final action names the KWin path it went out on
    /// rather than expiring as a generic timeout.
    #[test]
    fn a_post_leave_final_action_deadline_has_a_distinct_reason() {
        let now = Instant::now();
        let mut transfer = awaiting_decision(now);
        transfer
            .drop_decision(
                DropDecision {
                    delivery_id: DeliveryId(9),
                    decision: DropDecisionKind::Copy,
                },
                now,
                POST_DECISION,
            )
            .unwrap();
        assert!(
            transfer
                .final_actions_sent(5, true, now)
                .unwrap()
                .is_empty()
        );

        let effects = transfer.check_deadline(now + POST_DECISION).unwrap();
        assert_eq!(
            terminal_reason(&effects),
            Some(TerminalReason::PostDropFinalActionDeadlineExpired)
        );
    }

    #[test]
    fn a_conforming_final_action_path_expires_as_a_plain_post_decision_timeout() {
        let now = Instant::now();
        let mut transfer = awaiting_decision(now);
        transfer
            .drop_decision(
                DropDecision {
                    delivery_id: DeliveryId(9),
                    decision: DropDecisionKind::Copy,
                },
                now,
                POST_DECISION,
            )
            .unwrap();
        assert!(
            transfer
                .final_actions_sent(5, false, now)
                .unwrap()
                .is_empty()
        );

        assert_eq!(
            terminal_reason(&transfer.check_deadline(now + POST_DECISION).unwrap()),
            Some(TerminalReason::PostDecisionDeadlineExpired)
        );
    }

    #[test]
    fn an_acknowledged_post_drop_action_expires_as_an_app_timeout() {
        let now = Instant::now();
        let mut transfer = awaiting_decision(now);
        transfer
            .drop_decision(
                DropDecision {
                    delivery_id: DeliveryId(9),
                    decision: DropDecisionKind::Copy,
                },
                now,
                POST_DECISION,
            )
            .unwrap();
        assert!(
            transfer
                .final_actions_sent(5, true, now)
                .unwrap()
                .is_empty()
        );
        assert!(
            transfer
                .final_action_barrier_done(5, Some(DndAction::Copy), now)
                .unwrap()
                .is_empty()
        );

        assert_eq!(
            terminal_reason(&transfer.check_deadline(now + POST_DECISION).unwrap()),
            Some(TerminalReason::PostDecisionDeadlineExpired)
        );
    }

    #[test]
    fn a_barrier_arriving_after_post_decision_expiry_cannot_complete_the_ask() {
        let now = Instant::now();
        let mut transfer = awaiting_decision(now);
        transfer
            .drop_decision(
                DropDecision {
                    delivery_id: DeliveryId(9),
                    decision: DropDecisionKind::Copy,
                },
                now,
                POST_DECISION,
            )
            .unwrap();
        assert!(
            transfer
                .final_actions_sent(5, false, now)
                .unwrap()
                .is_empty()
        );

        let effects = transfer
            .final_action_barrier_done(5, Some(DndAction::Copy), now + POST_DECISION)
            .unwrap();
        assert_eq!(
            terminal_reason(&effects),
            Some(TerminalReason::PostDecisionDeadlineExpired)
        );
    }

    #[test]
    fn app_completion_after_post_decision_expiry_is_terminal_expiry() {
        let now = Instant::now();
        let mut transfer = ready(1, DndAction::Copy, 1, now);
        transfer
            .physical_drop(TransportRevision(1), now, FENCE)
            .unwrap();
        assert!(dropped_event(&transfer.resolve_drop_fence(now).unwrap()).is_some());

        let effects = transfer
            .drop_complete(
                DropComplete {
                    delivery_id: DeliveryId(9),
                    outcome: DropOutcome::Completed(DndAction::Copy),
                },
                now + POST_DECISION,
            )
            .unwrap();
        assert_eq!(
            terminal_reason(&effects),
            Some(TerminalReason::PostDecisionDeadlineExpired)
        );
    }

    #[test]
    fn a_dismissed_ask_terminates_immediately() {
        let now = Instant::now();
        let mut transfer = awaiting_decision(now);
        let effects = transfer
            .drop_decision(
                DropDecision {
                    delivery_id: DeliveryId(9),
                    decision: DropDecisionKind::Dismissed,
                },
                now,
                POST_DECISION,
            )
            .unwrap();
        assert_eq!(
            terminal_reason(&effects),
            Some(TerminalReason::AppDismissed)
        );
    }

    #[test]
    fn an_ask_finishes_only_once_both_the_compositor_and_the_app_agree() {
        // wayland.xml wl_data_offer.set_actions/finish requires both the final
        // negotiated action and successful destination consumption.
        let now = Instant::now();
        let mut transfer = awaiting_decision(now);
        transfer
            .drop_decision(
                DropDecision {
                    delivery_id: DeliveryId(9),
                    decision: DropDecisionKind::Move,
                },
                now,
                POST_DECISION,
            )
            .unwrap();
        assert!(
            transfer
                .final_actions_sent(5, true, now)
                .unwrap()
                .is_empty()
        );

        // A callback dispatched before the sync barrier cannot acknowledge it.
        assert!(
            transfer
                .compositor_action(Some(DndAction::Move), now)
                .unwrap()
                .is_empty()
        );
        assert!(matches!(
            transfer.ask_phase(),
            AskPhase::AwaitingCompletion {
                action_acknowledged: false,
                ..
            }
        ));
        assert!(
            transfer
                .final_action_barrier_done(5, Some(DndAction::Move), now)
                .unwrap()
                .is_empty(),
            "the app has not completed yet"
        );
        assert!(matches!(
            transfer.ask_phase(),
            AskPhase::AwaitingCompletion {
                action_acknowledged: true,
                ..
            }
        ));

        let effects = transfer
            .drop_complete(
                DropComplete {
                    delivery_id: DeliveryId(9),
                    outcome: DropOutcome::Completed(DndAction::Move),
                },
                now,
            )
            .unwrap();
        assert_eq!(terminal_reason(&effects), Some(TerminalReason::Completed));
        assert!(matches!(effects[0], ReceiveEffect::FinishOffer));

        let mut app_first = awaiting_decision(now);
        app_first
            .drop_decision(
                DropDecision {
                    delivery_id: DeliveryId(9),
                    decision: DropDecisionKind::Move,
                },
                now,
                POST_DECISION,
            )
            .unwrap();
        app_first.final_actions_sent(6, true, now).unwrap();
        assert!(
            app_first
                .drop_complete(
                    DropComplete {
                        delivery_id: DeliveryId(9),
                        outcome: DropOutcome::Completed(DndAction::Move),
                    },
                    now,
                )
                .unwrap()
                .is_empty(),
            "app-first completion waits for the compositor"
        );
        assert!(matches!(
            app_first.ask_phase(),
            AskPhase::AwaitingCompletion {
                action_acknowledged: false,
                app_completed: true,
                ..
            }
        ));
        assert!(
            app_first
                .final_action_barrier_done(6, Some(DndAction::Move), now)
                .unwrap()
                .is_empty(),
            "the latch resolves only at end-of-batch settlement"
        );
        let effects = app_first.settle_completion(now).unwrap();
        assert_eq!(terminal_reason(&effects), Some(TerminalReason::Completed));
    }

    /// Once the final `set_actions` is out, a `None` action means negotiation
    /// has definitively failed; waiting out the deadline reaches the same
    /// terminal minutes later while holding the offer.
    #[test]
    fn the_receive_state_machine_fails_a_final_none_action_fast() {
        // wayland.xml wl_data_offer.action: none after the destination's final
        // set_actions means that negotiation selected no action.
        let now = Instant::now();
        let mut transfer = awaiting_decision(now);
        transfer
            .drop_decision(
                DropDecision {
                    delivery_id: DeliveryId(9),
                    decision: DropDecisionKind::Move,
                },
                now,
                POST_DECISION,
            )
            .unwrap();
        assert!(
            transfer
                .final_actions_sent(5, true, now)
                .unwrap()
                .is_empty()
        );

        let effects = transfer.final_action_barrier_done(5, None, now).unwrap();
        assert_eq!(
            terminal_reason(&effects),
            Some(TerminalReason::FinalActionRejected)
        );
    }

    /// Before the final request there is no latch, so mid-negotiation churn —
    /// including `None` — is ordinary and must not terminate anything.
    #[test]
    fn a_none_action_during_hover_is_ordinary_churn() {
        // wayland.xml wl_data_offer.action may change repeatedly as modifiers
        // and destination set_actions requests change during hover.
        let now = Instant::now();
        let mut transfer = ready(1, DndAction::Copy, 1, now);
        assert!(transfer.compositor_action(None, now).unwrap().is_empty());
        assert!(transfer.terminal_event().is_none());
    }

    // ---- payload and failure paths --------------------------------------

    #[test]
    fn each_payload_failure_carries_its_own_terminal_reason() {
        for (failure, expected) in [
            (PayloadFailure::TooLarge, TerminalReason::PayloadTooLarge),
            (
                PayloadFailure::Inactive,
                TerminalReason::PayloadInactivityExpired,
            ),
            (PayloadFailure::Pipe, TerminalReason::PipeFailure),
        ] {
            let now = Instant::now();
            let mut transfer = transfer();
            transfer
                .accept(context(1, DndAction::Copy), TransportRevision(1))
                .unwrap();
            assert!(transfer.begin_fetch(now).unwrap().is_empty());

            let effects = transfer.payload_ready(ID, Err(failure), now).unwrap();
            assert_eq!(terminal_reason(&effects), Some(expected));
            assert_eq!(transfer.terminal_transition_count(), 1);
            assert!(!transfer.resources().active_transfer);
        }
    }

    /// The drop is emitted only once *both* halves are in, whichever order
    /// they arrive in.
    #[test]
    fn a_drop_waits_for_its_payload() {
        // wayland.xml wl_data_offer.receive may complete before or after
        // wl_data_device.drop; the consumer drop requires both observations.
        let now = Instant::now();
        let mut drop_first = transfer();
        drop_first
            .accept(context(1, DndAction::Copy), TransportRevision(1))
            .unwrap();
        assert!(drop_first.begin_fetch(now).unwrap().is_empty());
        drop_first
            .physical_drop(TransportRevision(1), now, FENCE)
            .unwrap();

        let effects = drop_first.resolve_drop_fence(now).unwrap();
        assert!(dropped_event(&effects).is_none(), "payload not ready yet");
        assert!(drop_first.drop_pending());

        drop_first
            .compositor_action(Some(DndAction::Copy), now)
            .unwrap();
        let effects = drop_first.payload_ready(ID, Ok(payload()), now).unwrap();
        assert!(dropped_event(&effects).is_some());

        let mut payload_first = transfer();
        payload_first
            .accept(context(1, DndAction::Copy), TransportRevision(1))
            .unwrap();
        payload_first.begin_fetch(now).unwrap();
        assert!(
            payload_first
                .payload_ready(ID, Ok(payload()), now)
                .unwrap()
                .is_empty(),
            "payload readiness alone is not a drop"
        );
        payload_first
            .compositor_action(Some(DndAction::Copy), now)
            .unwrap();
        payload_first
            .physical_drop(TransportRevision(1), now, FENCE)
            .unwrap();
        assert!(
            dropped_event(&payload_first.resolve_drop_fence(now).unwrap()).is_some(),
            "drop after payload emits once the second half arrives"
        );
    }

    #[test]
    fn a_stale_transfer_id_is_rejected_rather_than_applied() {
        let now = Instant::now();
        let mut transfer = transfer();
        assert_eq!(
            transfer.fail(DataTransferId(99), TerminalReason::OfferRejected, now),
            Err(ReceiveError::StaleTransfer {
                expected: ID,
                received: DataTransferId(99),
            })
        );
        assert!(transfer.terminal_event().is_none());
    }

    #[test]
    fn an_observed_offer_replacement_wins_over_an_unobserved_expiry() {
        let now = Instant::now();
        let mut transfer = ready(1, DndAction::Copy, 1, now);
        transfer
            .physical_drop(TransportRevision(1), now, FENCE)
            .unwrap();
        transfer.resolve_drop_fence(now).unwrap();

        let effects = transfer.offer_replaced(ID).unwrap();
        assert_eq!(
            terminal_reason(&effects),
            Some(TerminalReason::OfferReplaced)
        );
    }

    #[test]
    fn a_completed_action_disagreeing_with_the_snapshot_is_a_mismatch() {
        let now = Instant::now();
        let mut transfer = ready(1, DndAction::Copy, 1, now);
        transfer
            .physical_drop(TransportRevision(1), now, FENCE)
            .unwrap();
        transfer.resolve_drop_fence(now).unwrap();

        let effects = transfer
            .drop_complete(
                DropComplete {
                    delivery_id: DeliveryId(9),
                    outcome: DropOutcome::Completed(DndAction::Move),
                },
                now,
            )
            .unwrap();
        assert_eq!(
            terminal_reason(&effects),
            Some(TerminalReason::ActionMismatch)
        );
    }

    #[test]
    fn a_stale_drop_complete_cannot_satisfy_the_completion_latch() {
        let now = Instant::now();
        let mut transfer = ready(1, DndAction::Copy, 1, now);
        transfer
            .physical_drop(TransportRevision(1), now, FENCE)
            .unwrap();
        transfer.resolve_drop_fence(now).unwrap();

        assert_eq!(
            transfer.drop_complete(
                DropComplete {
                    delivery_id: DeliveryId(99),
                    outcome: DropOutcome::Completed(DndAction::Copy),
                },
                now
            ),
            Err(ReceiveError::DeliveryMismatch)
        );
        assert_eq!(
            terminal_reason(&transfer.check_deadline(now + POST_DECISION).unwrap()),
            Some(TerminalReason::PostDecisionDeadlineExpired)
        );
    }

    #[test]
    fn invalidating_the_snapshot_revision_fails_closed() {
        let now = Instant::now();
        let mut transfer = ready(1, DndAction::Copy, 1, now);
        transfer
            .physical_drop(TransportRevision(1), now, FENCE)
            .unwrap();
        transfer.resolve_drop_fence(now).unwrap();

        assert!(
            transfer
                .invalidate_revision(ProposalRevision(2), now)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            terminal_reason(
                &transfer
                    .invalidate_revision(ProposalRevision(1), now)
                    .unwrap()
            ),
            Some(TerminalReason::RevisionInvalidated)
        );
    }

    #[test]
    fn losing_the_accepted_target_fails_closed() {
        let now = Instant::now();
        let mut transfer = ready(3, DndAction::Copy, 1, now);
        assert!(transfer.target_lost(TargetId(4), now).unwrap().is_empty());
        assert_eq!(
            terminal_reason(&transfer.target_lost(TargetId(3), now).unwrap()),
            Some(TerminalReason::TargetLost)
        );
    }

    #[test]
    fn an_unrepresentable_deadline_saturates_instead_of_expiring_immediately() {
        let now = Instant::now();
        let deadline = deadline_after(now, Duration::MAX);
        assert!(deadline.at > now);
        assert!(deadline.at.checked_add(Duration::from_nanos(1)).is_none());
    }

    #[test]
    fn a_transport_correlated_echo_accepts_only_its_internal_source_origin() {
        let mut echo = transfer();
        let expected = DndOrigin::Internal(SourceId(7));
        let mut accepted = context(3, DndAction::Copy);
        accepted.origin = expected;
        assert_eq!(
            echo.accept_for_origin(accepted, TransportRevision(1), expected),
            Ok(())
        );

        let mut wrong = transfer();
        let mut accepted = context(3, DndAction::Copy);
        accepted.origin = DndOrigin::Internal(SourceId(8));
        assert_eq!(
            wrong.accept_for_origin(accepted, TransportRevision(1), expected),
            Err(ReceiveError::OriginMismatch)
        );
    }
}
