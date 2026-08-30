//! Deterministic `dialog.v1` lifecycle, capacity, and ownership policy.
//!
//! Memory is bounded in three independently accounted pools: the fair waiting
//! queue, active presenter attempts, and retained terminal records. Presenter
//! replacement may need to move an active record back to a full waiting queue;
//! the in-flight record wins and the oldest already-queued record fails.
//! Terminal count is enforced synchronously, while [`DialogBroker::gc`] applies
//! injected-time expiry and TTL retention for the Phase-3 scheduler.
//! Presenter `resolve` can never fabricate a progress outcome. The narrower
//! permanent-render-failure escape remains valid for every kind, including
//! progress: [`DialogBroker::fail_presentation`] may terminal it as `Failed`.

use std::collections::{HashMap, VecDeque};
use std::fmt;

use subtle::ConstantTimeEq;

use cosmix_interaction_schema::{
    DialogOpenRequestV1, DialogOpenResponse, DialogProgressCompletionV1, DialogProgressPatchV1,
    DialogProgressValueV1, DialogRequestV1, DialogResultResponseV1, DialogStateV1,
    DialogValidationError, DialogValueV1, InteractionHandle, OwnerToken,
};

use crate::RateLimiter;

pub const DEFAULT_DIALOG_QUEUE_LIMIT: usize = 64;
pub const DEFAULT_DIALOG_ORIGIN_QUEUE_LIMIT: usize = 8;
pub const DEFAULT_DIALOG_ACTIVE_LIMIT: usize = 1;
pub const DEFAULT_DIALOG_TERMINAL_LIMIT: usize = 256;
pub const DEFAULT_DIALOG_TERMINAL_TTL_MS: u64 = 15 * 60 * 1_000;
/// Maximum pre-display requeues tolerated before a never-presented dialog is
/// quarantined. Three retries absorb transient renderer startup failures; the
/// fourth requeue terminates a repeatable poison pill.
pub const MAX_DIALOG_PRE_DISPLAY_REQUEUES: u8 = 3;
// Implicit maintenance can emit two edges for every resident record (expiry
// followed by immediate eviction). Reserve two further slots for the triggering
// public operation's own state edge and possible synchronous eviction edge.
const DIALOG_TRANSITION_MUTATION_HEADROOM: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DialogQueueLimits {
    pub total: usize,
    pub per_origin: usize,
}

impl Default for DialogQueueLimits {
    fn default() -> Self {
        Self {
            total: DEFAULT_DIALOG_QUEUE_LIMIT,
            per_origin: DEFAULT_DIALOG_ORIGIN_QUEUE_LIMIT,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DialogRetentionPolicy {
    pub active: usize,
    pub terminal: usize,
    pub terminal_ttl_ms: u64,
}

impl Default for DialogRetentionPolicy {
    fn default() -> Self {
        Self {
            active: DEFAULT_DIALOG_ACTIVE_LIMIT,
            terminal: DEFAULT_DIALOG_TERMINAL_LIMIT,
            terminal_ttl_ms: DEFAULT_DIALOG_TERMINAL_TTL_MS,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresenterLease {
    service: String,
    generation: u64,
    instance_epoch: u64,
}

impl PresenterLease {
    #[must_use]
    pub fn service(&self) -> &str {
        &self.service
    }

    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    #[must_use]
    pub fn instance_epoch(&self) -> u64 {
        self.instance_epoch
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PresentationAttemptToken(u64);

impl PresentationAttemptToken {
    #[must_use]
    pub fn as_u64(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DialogPresentation {
    pub handle: InteractionHandle,
    pub attempt_token: PresentationAttemptToken,
    pub request: DialogOpenRequestV1,
    pub progress: Option<DialogProgressSnapshot>,
    pub cancel_requested: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DialogProgressSnapshot {
    pub message: Option<String>,
    pub progress: DialogProgressValueV1,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DialogSnapshot {
    pub handle: InteractionHandle,
    pub owner_service: String,
    pub request: DialogOpenRequestV1,
    pub state: DialogStateV1,
    pub created_at_ms: u64,
    pub expires_at_ms: Option<u64>,
    pub terminal_at_ms: Option<u64>,
    pub progress: Option<DialogProgressSnapshot>,
    pub cancel_requested: bool,
    pub value: Option<DialogValueV1>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DialogBrokerError {
    Invalid(DialogValidationError),
    RateLimited,
    QueueFull { max: usize },
    OriginQueueFull { max: usize },
    DuplicateHandle,
    NotFound,
    WrongOwner,
    NoPresenter,
    StaleLease,
    StaleAttempt,
    CounterExhausted,
    AlreadyTerminal,
    Expired,
    InvalidState(DialogStateV1),
    OwnerCannotResolve,
    ProgressPresenterResolution,
    NotProgress,
    NotCancellable,
}

impl fmt::Display for DialogBrokerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(error) => write!(f, "invalid dialog: {error}"),
            Self::RateLimited => write!(f, "dialog-open rate limit exceeded"),
            Self::QueueFull { max } => write!(f, "dialog queue is full ({max})"),
            Self::OriginQueueFull { max } => {
                write!(f, "dialog origin queue is full ({max})")
            }
            Self::DuplicateHandle => write!(f, "interaction handle already exists"),
            Self::NotFound => write!(f, "dialog not found"),
            Self::WrongOwner => write!(f, "dialog owner service or token does not match"),
            Self::NoPresenter => write!(f, "no presenter lease is registered"),
            Self::StaleLease => write!(f, "presenter lease is stale"),
            Self::StaleAttempt => write!(f, "presentation attempt is stale"),
            Self::CounterExhausted => write!(f, "broker generation counter is exhausted"),
            Self::AlreadyTerminal => write!(f, "dialog is already terminal"),
            Self::Expired => write!(f, "dialog deadline has expired"),
            Self::InvalidState(state) => write!(f, "invalid dialog state: {state:?}"),
            Self::OwnerCannotResolve => write!(f, "a dialog owner cannot resolve its own dialog"),
            Self::ProgressPresenterResolution => {
                write!(f, "only the owner may complete a progress dialog")
            }
            Self::NotProgress => write!(f, "dialog is not a progress dialog"),
            Self::NotCancellable => write!(f, "progress dialog is not cancellable"),
        }
    }
}

impl std::error::Error for DialogBrokerError {}

impl From<DialogValidationError> for DialogBrokerError {
    fn from(value: DialogValidationError) -> Self {
        Self::Invalid(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ActiveAttempt {
    generation: u64,
    token: PresentationAttemptToken,
}

#[derive(Debug, Clone)]
struct DialogRecord {
    handle: InteractionHandle,
    owner_service: String,
    owner_token: OwnerToken,
    request: DialogOpenRequestV1,
    state: DialogStateV1,
    created_at_ms: u64,
    expires_at_ms: Option<u64>,
    terminal_at_ms: Option<u64>,
    attempt: Option<ActiveAttempt>,
    progress: Option<DialogProgressSnapshot>,
    cancel_requested: bool,
    ever_presented: bool,
    pre_display_requeues: u8,
    value: Option<DialogValueV1>,
}

#[derive(Debug)]
pub struct DialogBroker {
    rate_limiter: RateLimiter,
    limits: DialogQueueLimits,
    instance_epoch: u64,
    records: HashMap<InteractionHandle, DialogRecord>,
    queued_by_origin: HashMap<String, VecDeque<InteractionHandle>>,
    origin_turns: VecDeque<String>,
    presenter: Option<PresenterLease>,
    next_presenter_generation: u64,
    next_attempt_token: u64,
    retention: DialogRetentionPolicy,
    terminal_order: VecDeque<InteractionHandle>,
    pending_expirations: VecDeque<InteractionHandle>,
    pending_count_evictions: VecDeque<InteractionHandle>,
    pending_transitions: Vec<DialogTransition>,
    transition_limit: usize,
    transitions_overflowed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DialogGcOutcome {
    pub expired: Vec<InteractionHandle>,
    pub evicted: Vec<InteractionHandle>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogTransitionCause {
    Open,
    Present,
    MarkPresented,
    Resolve,
    Fail,
    Cancel,
    ProgressUpdate,
    ProgressComplete,
    ProgressCancel,
    Replace,
    Release,
    Expire,
    Evict,
    Withdraw,
    Quarantine,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DialogTransition {
    pub handle: InteractionHandle,
    pub from: Option<DialogStateV1>,
    pub to: Option<DialogStateV1>,
    pub cause: DialogTransitionCause,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DialogTransitionBatch {
    pub transitions: Vec<DialogTransition>,
    pub overflowed: bool,
}

/// Deterministic broker default with presenter instance epoch zero.
///
/// Epoch zero provides no restart fencing. Production must inject a
/// restart-unique, unpredictable CSPRNG value through
/// [`DialogBroker::with_instance_epoch`]; repeating an epoch across restarts
/// allows a pre-restart presenter lease to collide with a newly minted lease.
impl Default for DialogBroker {
    fn default() -> Self {
        Self::new(RateLimiter::default(), DialogQueueLimits::default())
    }
}

impl DialogBroker {
    /// Construct a deterministic broker with presenter instance epoch zero.
    ///
    /// Epoch zero provides no restart fencing. Production must call
    /// [`Self::with_instance_epoch`] with a restart-unique, unpredictable CSPRNG
    /// value. Repeating an epoch across restarts breaks stale-lease fencing.
    #[must_use]
    pub fn new(rate_limiter: RateLimiter, limits: DialogQueueLimits) -> Self {
        Self::new_with_retention(rate_limiter, limits, DialogRetentionPolicy::default())
    }

    /// Construct a deterministic broker with custom retention and epoch zero.
    ///
    /// Epoch zero provides no restart fencing. Production must call
    /// [`Self::with_instance_epoch`] with a restart-unique, unpredictable CSPRNG
    /// value. Repeating an epoch across restarts breaks stale-lease fencing.
    #[must_use]
    pub fn new_with_retention(
        rate_limiter: RateLimiter,
        limits: DialogQueueLimits,
        retention: DialogRetentionPolicy,
    ) -> Self {
        // Implicit maintenance may consume two edges for every resident queued,
        // active, or terminal record. Headroom keeps the triggering operation's
        // own bounded edge(s) observable. A sustained undrained mutation storm
        // remains bounded and is surfaced as an overflow resync.
        let transition_limit = limits
            .total
            .saturating_add(retention.active)
            .saturating_add(retention.terminal)
            .saturating_mul(2)
            .saturating_add(DIALOG_TRANSITION_MUTATION_HEADROOM);
        Self {
            rate_limiter,
            limits,
            instance_epoch: 0,
            records: HashMap::new(),
            queued_by_origin: HashMap::new(),
            origin_turns: VecDeque::new(),
            presenter: None,
            next_presenter_generation: 1,
            next_attempt_token: 1,
            retention,
            terminal_order: VecDeque::new(),
            pending_expirations: VecDeque::new(),
            pending_count_evictions: VecDeque::new(),
            pending_transitions: Vec::new(),
            transition_limit,
            transitions_overflowed: false,
        }
    }

    /// Stamp future presenter leases with a production daemon-instance epoch.
    ///
    /// Stage-2 production code must draw a [`std::num::NonZeroU64`] once from a
    /// CSPRNG at every process startup. The type excludes epoch zero; the only
    /// remaining collision risk is repeating the same 64-bit draw across
    /// restarts (approximately 1 in 2^64 without persistence). Existing
    /// constructors and [`Default`] intentionally retain deterministic epoch
    /// zero for tests, where restart fencing is disabled.
    #[must_use]
    pub fn with_instance_epoch(mut self, instance_epoch: std::num::NonZeroU64) -> Self {
        self.instance_epoch = instance_epoch.get();
        self
    }

    pub fn open(
        &mut self,
        owner_service: &str,
        owner_token: OwnerToken,
        request: DialogOpenRequestV1,
        now_ms: u64,
        fresh_handle: InteractionHandle,
    ) -> Result<DialogOpenResponse, DialogBrokerError> {
        let _ = self.drain_pending_report();
        request.validate()?;
        self.maintain(now_ms);
        if self.records.contains_key(&fresh_handle) {
            return Err(DialogBrokerError::DuplicateHandle);
        }
        let queued_for_origin = self
            .queued_by_origin
            .get(owner_service)
            .map_or(0, VecDeque::len);
        if self.queued_len() >= self.limits.total {
            return Err(DialogBrokerError::QueueFull {
                max: self.limits.total,
            });
        }
        if queued_for_origin >= self.limits.per_origin {
            return Err(DialogBrokerError::OriginQueueFull {
                max: self.limits.per_origin,
            });
        }
        if !self.rate_limiter.try_consume(owner_service, now_ms) {
            return Err(DialogBrokerError::RateLimited);
        }

        let expires_at_ms = request
            .deadline_ms
            .map(|duration| now_ms.saturating_add(duration));
        let progress = initial_progress(&request.dialog);
        let record = DialogRecord {
            handle: fresh_handle.clone(),
            owner_service: owner_service.to_owned(),
            owner_token: owner_token.clone(),
            request,
            state: DialogStateV1::Queued,
            created_at_ms: now_ms,
            expires_at_ms,
            terminal_at_ms: None,
            attempt: None,
            progress,
            cancel_requested: false,
            ever_presented: false,
            pre_display_requeues: 0,
            value: None,
        };
        self.records.insert(fresh_handle.clone(), record);
        self.enqueue(owner_service, fresh_handle.clone());
        self.record_transition(
            fresh_handle.clone(),
            None,
            Some(DialogStateV1::Queued),
            DialogTransitionCause::Open,
        );
        Ok(DialogOpenResponse {
            handle: fresh_handle,
            owner_token,
            state: DialogStateV1::Queued,
        })
    }

    /// Install a broker-minted presenter generation.
    ///
    /// Registration always advances the generation, even when the service name
    /// is unchanged. Active work from the previous generation is requeued. If
    /// that would overflow a waiting-queue cap, the oldest already-queued
    /// dialog is failed to preserve the in-flight dialog.
    pub fn register_presenter(
        &mut self,
        service: impl Into<String>,
        now_ms: u64,
    ) -> Result<PresenterLease, DialogBrokerError> {
        let _ = self.drain_pending_report();
        self.maintain(now_ms);
        let generation = self.mint_presenter_generation()?;
        let lease = PresenterLease {
            service: service.into(),
            generation,
            instance_epoch: self.instance_epoch,
        };
        let active_generation = self.presenter.as_ref().map(|value| value.generation);
        if let Some(active_generation) = active_generation {
            self.requeue_generation(active_generation, now_ms, DialogTransitionCause::Replace);
        }
        self.presenter = Some(lease.clone());
        Ok(lease)
    }

    pub fn release_presenter(
        &mut self,
        lease: &PresenterLease,
        now_ms: u64,
    ) -> Result<(), DialogBrokerError> {
        let _ = self.drain_pending_report();
        self.check_lease(lease)?;
        self.maintain(now_ms);
        self.requeue_generation(lease.generation, now_ms, DialogTransitionCause::Release);
        self.presenter = None;
        Ok(())
    }

    pub fn next_presentation(
        &mut self,
        lease: &PresenterLease,
        now_ms: u64,
    ) -> Result<Option<DialogPresentation>, DialogBrokerError> {
        let _ = self.drain_pending_report();
        self.check_lease(lease)?;
        self.maintain(now_ms);
        if self.active_len() >= self.retention.active {
            return Ok(None);
        }
        let Some(handle) = self.dequeue_fair() else {
            return Ok(None);
        };
        let attempt_token = self.mint_attempt_token()?;
        let presentation = {
            let record = self
                .records
                .get_mut(&handle)
                .expect("queued handle must have a record");
            record.state = DialogStateV1::Presenting;
            record.attempt = Some(ActiveAttempt {
                generation: lease.generation,
                token: attempt_token,
            });
            DialogPresentation {
                handle: handle.clone(),
                attempt_token,
                request: record.request.clone(),
                progress: record.progress.clone(),
                cancel_requested: record.cancel_requested,
            }
        };
        self.record_transition(
            handle,
            Some(DialogStateV1::Queued),
            Some(DialogStateV1::Presenting),
            DialogTransitionCause::Present,
        );
        Ok(Some(presentation))
    }

    pub fn mark_presented(
        &mut self,
        lease: &PresenterLease,
        handle: &InteractionHandle,
        attempt_token: PresentationAttemptToken,
        now_ms: u64,
    ) -> Result<(), DialogBrokerError> {
        let _ = self.drain_pending_report();
        self.check_lease(lease)?;
        self.check_current_attempt(handle, lease.generation, attempt_token)?;
        if self.expire_handle_if_due(handle, now_ms) {
            return Err(DialogBrokerError::Expired);
        }
        let record = self
            .records
            .get_mut(handle)
            .expect("attempt check found record");
        if record.state != DialogStateV1::Presenting {
            return Err(if record.state.is_terminal() {
                DialogBrokerError::AlreadyTerminal
            } else {
                DialogBrokerError::InvalidState(record.state)
            });
        }
        let new_state = if record.cancel_requested {
            DialogStateV1::CancelRequested
        } else {
            DialogStateV1::Presented
        };
        record.state = new_state;
        record.ever_presented = true;
        self.record_transition(
            handle.clone(),
            Some(DialogStateV1::Presenting),
            Some(new_state),
            DialogTransitionCause::MarkPresented,
        );
        Ok(())
    }

    pub fn resolve(
        &mut self,
        lease: &PresenterLease,
        handle: &InteractionHandle,
        attempt_token: PresentationAttemptToken,
        value: DialogValueV1,
        now_ms: u64,
    ) -> Result<(), DialogBrokerError> {
        let _ = self.drain_pending_report();
        self.check_lease(lease)?;
        self.check_current_attempt(handle, lease.generation, attempt_token)?;
        if self.expire_handle_if_due(handle, now_ms) {
            return Err(DialogBrokerError::Expired);
        }
        let record = self
            .records
            .get_mut(handle)
            .expect("attempt check found record");
        record.attempt.take();
        let rejection = if record.owner_service == lease.service {
            Some(DialogBrokerError::OwnerCannotResolve)
        } else if record.state.is_terminal() {
            Some(DialogBrokerError::AlreadyTerminal)
        } else if !matches!(
            record.state,
            DialogStateV1::Presented | DialogStateV1::CancelRequested
        ) {
            Some(DialogBrokerError::InvalidState(record.state))
        } else if matches!(record.request.dialog, DialogRequestV1::Progress { .. }) {
            Some(DialogBrokerError::ProgressPresenterResolution)
        } else {
            value
                .validate_for(&record.request.dialog)
                .err()
                .map(DialogBrokerError::Invalid)
        };
        if let Some(error) = rejection {
            self.requeue_protected(vec![handle.clone()], now_ms, DialogTransitionCause::Resolve);
            return Err(error);
        }
        self.transition_terminal(
            handle,
            DialogStateV1::Resolved,
            Some(value),
            now_ms,
            DialogTransitionCause::Resolve,
        );
        Ok(())
    }

    /// Explicit integration guard: possession of the owner capability permits
    /// lifecycle mutation and result reads, but never modal decision injection.
    pub fn resolve_as_owner(
        &self,
        owner_service: &str,
        owner_token: &OwnerToken,
        handle: &InteractionHandle,
    ) -> Result<(), DialogBrokerError> {
        self.check_owner(owner_service, owner_token, handle)?;
        Err(DialogBrokerError::OwnerCannotResolve)
    }

    /// Permanently fail an attempt that the presenter cannot render.
    ///
    /// This is valid for every dialog kind, including progress. It records only
    /// the transport/presentation outcome `Failed`; it cannot fabricate a
    /// progress success or cancellation value.
    pub fn fail_presentation(
        &mut self,
        lease: &PresenterLease,
        handle: &InteractionHandle,
        attempt_token: PresentationAttemptToken,
        now_ms: u64,
    ) -> Result<(), DialogBrokerError> {
        let _ = self.drain_pending_report();
        self.check_lease(lease)?;
        self.check_current_attempt(handle, lease.generation, attempt_token)?;
        if self.expire_handle_if_due(handle, now_ms) {
            return Err(DialogBrokerError::Expired);
        }
        let record = self
            .records
            .get_mut(handle)
            .expect("attempt check found record");
        record.attempt.take();
        if record.state.is_terminal() {
            return Err(DialogBrokerError::AlreadyTerminal);
        }
        self.transition_terminal(
            handle,
            DialogStateV1::Failed,
            None,
            now_ms,
            DialogTransitionCause::Fail,
        );
        Ok(())
    }

    pub fn cancel(
        &mut self,
        owner_service: &str,
        owner_token: &OwnerToken,
        handle: &InteractionHandle,
        now_ms: u64,
    ) -> Result<(), DialogBrokerError> {
        let _ = self.drain_pending_report();
        self.check_owner(owner_service, owner_token, handle)?;
        if self.expire_handle_if_due(handle, now_ms) {
            return Err(DialogBrokerError::Expired);
        }
        let state = self
            .records
            .get(handle)
            .expect("owner check found record")
            .state;
        if state.is_terminal() {
            return Err(DialogBrokerError::AlreadyTerminal);
        }
        self.transition_terminal(
            handle,
            DialogStateV1::Cancelled,
            None,
            now_ms,
            DialogTransitionCause::Cancel,
        );
        Ok(())
    }

    pub fn request_progress_cancel(
        &mut self,
        lease: &PresenterLease,
        handle: &InteractionHandle,
        attempt_token: PresentationAttemptToken,
        now_ms: u64,
    ) -> Result<(), DialogBrokerError> {
        let _ = self.drain_pending_report();
        self.check_lease(lease)?;
        self.check_current_attempt(handle, lease.generation, attempt_token)?;
        if self.expire_handle_if_due(handle, now_ms) {
            return Err(DialogBrokerError::Expired);
        }
        let record = self
            .records
            .get_mut(handle)
            .expect("attempt check found record");
        let DialogRequestV1::Progress { cancellable, .. } = &record.request.dialog else {
            return Err(DialogBrokerError::NotProgress);
        };
        if !cancellable {
            return Err(DialogBrokerError::NotCancellable);
        }
        if record.state == DialogStateV1::CancelRequested {
            return Ok(());
        }
        if record.state != DialogStateV1::Presented {
            return Err(if record.state.is_terminal() {
                DialogBrokerError::AlreadyTerminal
            } else {
                DialogBrokerError::InvalidState(record.state)
            });
        }
        record.cancel_requested = true;
        record.state = DialogStateV1::CancelRequested;
        self.record_transition(
            handle.clone(),
            Some(DialogStateV1::Presented),
            Some(DialogStateV1::CancelRequested),
            DialogTransitionCause::ProgressCancel,
        );
        Ok(())
    }

    pub fn update_progress(
        &mut self,
        owner_service: &str,
        owner_token: &OwnerToken,
        handle: &InteractionHandle,
        patch: DialogProgressPatchV1,
        now_ms: u64,
    ) -> Result<(), DialogBrokerError> {
        let _ = self.drain_pending_report();
        self.check_owner(owner_service, owner_token, handle)?;
        if self.expire_handle_if_due(handle, now_ms) {
            return Err(DialogBrokerError::Expired);
        }
        patch.validate()?;
        let record = self
            .records
            .get_mut(handle)
            .expect("owner check found record");
        if record.state.is_terminal() {
            return Err(DialogBrokerError::AlreadyTerminal);
        }
        let Some(progress) = &mut record.progress else {
            return Err(DialogBrokerError::NotProgress);
        };
        let message_changed = patch
            .message
            .as_ref()
            .is_some_and(|message| progress.message.as_ref() != Some(message));
        let value_changed = patch
            .progress
            .as_ref()
            .is_some_and(|value| progress.progress != *value);
        if !message_changed && !value_changed {
            return Ok(());
        }
        if let Some(message) = patch.message {
            progress.message = Some(message);
        }
        if let Some(value) = patch.progress {
            progress.progress = value;
        }
        let state = record.state;
        self.record_transition(
            handle.clone(),
            Some(state),
            Some(state),
            DialogTransitionCause::ProgressUpdate,
        );
        Ok(())
    }

    pub fn complete_progress(
        &mut self,
        owner_service: &str,
        owner_token: &OwnerToken,
        handle: &InteractionHandle,
        completion: DialogProgressCompletionV1,
        now_ms: u64,
    ) -> Result<(), DialogBrokerError> {
        let _ = self.drain_pending_report();
        self.check_owner(owner_service, owner_token, handle)?;
        if self.expire_handle_if_due(handle, now_ms) {
            return Err(DialogBrokerError::Expired);
        }
        completion.validate()?;
        let record = self.records.get(handle).expect("owner check found record");
        if record.state.is_terminal() {
            return Err(DialogBrokerError::AlreadyTerminal);
        }
        if record.progress.is_none() {
            return Err(DialogBrokerError::NotProgress);
        }
        self.transition_terminal(
            handle,
            DialogStateV1::Resolved,
            Some(DialogValueV1::Progress { completion }),
            now_ms,
            DialogTransitionCause::ProgressComplete,
        );
        Ok(())
    }

    pub fn result(
        &self,
        owner_service: &str,
        owner_token: &OwnerToken,
        handle: &InteractionHandle,
    ) -> Result<DialogResultResponseV1, DialogBrokerError> {
        self.check_owner(owner_service, owner_token, handle)?;
        let record = self.records.get(handle).expect("owner check found record");
        Ok(DialogResultResponseV1 {
            handle: handle.clone(),
            state: record.state,
            value: record.value.clone(),
        })
    }

    #[must_use]
    pub fn snapshot(&self, handle: &InteractionHandle) -> Option<DialogSnapshot> {
        self.records.get(handle).map(snapshot_from_record)
    }

    /// Earliest absolute time at which deadline expiry or terminal TTL
    /// retention can change broker state.
    #[must_use]
    pub fn next_maintenance_at_ms(&self) -> Option<u64> {
        let next_expiry = self
            .records
            .values()
            .filter(|record| !record.state.is_terminal())
            .filter_map(|record| record.expires_at_ms)
            .min();
        let next_eviction = self
            .records
            .values()
            .filter(|record| record.state.is_terminal())
            .filter_map(|record| {
                record
                    .terminal_at_ms
                    .and_then(|at| at.checked_add(self.retention.terminal_ttl_ms))
            })
            .min();
        match (next_expiry, next_eviction) {
            (Some(expiry), Some(eviction)) => Some(expiry.min(eviction)),
            (Some(expiry), None) => Some(expiry),
            (None, Some(eviction)) => Some(eviction),
            (None, None) => None,
        }
    }

    /// Drain bounded lifecycle edges accumulated by broker mutations.
    ///
    /// `overflowed` requires a consumer to discard the incremental batch and
    /// reseed from a full snapshot.
    pub fn drain_transitions(&mut self) -> DialogTransitionBatch {
        DialogTransitionBatch {
            transitions: std::mem::take(&mut self.pending_transitions),
            overflowed: std::mem::take(&mut self.transitions_overflowed),
        }
    }

    /// Force-fail every live dialog owned by a vanished service.
    pub fn withdraw_owner(&mut self, owner_service: &str, now_ms: u64) -> Vec<InteractionHandle> {
        let _ = self.drain_pending_report();
        self.maintain(now_ms);
        let mut handles: Vec<_> = self
            .records
            .values()
            .filter(|record| record.owner_service == owner_service && !record.state.is_terminal())
            .map(|record| record.handle.clone())
            .collect();
        handles.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        for handle in &handles {
            self.transition_terminal(
                handle,
                DialogStateV1::Failed,
                None,
                now_ms,
                DialogTransitionCause::Withdraw,
            );
        }
        handles
    }

    /// Expire due dialogs at injected `now_ms`.
    ///
    /// Phase 3 must also call [`Self::gc`] from its scheduler; transition-time
    /// deadline checks are authoritative, while this sweep supplies idle-time
    /// expiry and terminal-retention cleanup without polling.
    pub fn expire(&mut self, now_ms: u64) -> Vec<InteractionHandle> {
        let _ = self.drain_pending_report();
        self.expire_due(now_ms);
        self.pending_expirations.drain(..).collect()
    }

    /// Run the injected-time maintenance contract.
    ///
    /// The future daemon scheduler calls this on its event-driven wake and
    /// five-minute-or-longer backstop. It expires idle dialogs and evicts
    /// terminal records older than the TTL. Terminal count is also enforced
    /// synchronously at each terminal transition. The outcome drains the
    /// currently pending expirations and count/TTL evictions. Every mutating
    /// entry opportunistically drains an older pending report to keep memory
    /// bounded without scheduler cooperation, so Phase 3 calls `gc(now)`
    /// immediately after each mutation when it needs to publish every event.
    pub fn gc(&mut self, now_ms: u64) -> DialogGcOutcome {
        self.maintain(now_ms);
        self.drain_pending_report()
    }

    fn drain_pending_report(&mut self) -> DialogGcOutcome {
        let expired = self.pending_expirations.drain(..).collect();
        let evicted = self.pending_count_evictions.drain(..).collect();
        DialogGcOutcome { expired, evicted }
    }

    #[must_use]
    pub fn queued_len(&self) -> usize {
        self.queued_by_origin.values().map(VecDeque::len).sum()
    }

    #[must_use]
    pub fn active_len(&self) -> usize {
        self.records
            .values()
            .filter(|record| {
                matches!(
                    record.state,
                    DialogStateV1::Presenting
                        | DialogStateV1::Presented
                        | DialogStateV1::CancelRequested
                )
            })
            .count()
    }

    #[must_use]
    pub fn terminal_len(&self) -> usize {
        self.terminal_order.len()
    }

    fn maintain(&mut self, now_ms: u64) {
        self.expire_due(now_ms);
        let ttl_evictions = self.evict_terminal_ttl(now_ms);
        self.pending_count_evictions.extend(ttl_evictions);
    }

    fn expire_due(&mut self, now_ms: u64) {
        let expired: Vec<_> = self
            .records
            .values()
            .filter(|record| {
                !record.state.is_terminal()
                    && record
                        .expires_at_ms
                        .is_some_and(|deadline| now_ms >= deadline)
            })
            .map(|record| record.handle.clone())
            .collect();
        for handle in &expired {
            self.transition_terminal(
                handle,
                DialogStateV1::Expired,
                None,
                now_ms,
                DialogTransitionCause::Expire,
            );
        }
    }

    fn enqueue(&mut self, origin: &str, handle: InteractionHandle) {
        let queue = self.queued_by_origin.entry(origin.to_owned()).or_default();
        if queue.is_empty() {
            self.origin_turns.push_back(origin.to_owned());
        }
        queue.push_back(handle);
    }

    fn dequeue_fair(&mut self) -> Option<InteractionHandle> {
        while let Some(origin) = self.origin_turns.pop_front() {
            let (handle, has_more) = {
                let queue = self
                    .queued_by_origin
                    .get_mut(&origin)
                    .expect("origin turn must have a queue");
                (queue.pop_front(), !queue.is_empty())
            };
            if has_more {
                self.origin_turns.push_back(origin.clone());
            } else {
                self.queued_by_origin.remove(&origin);
            }
            if let Some(handle) = handle
                && self
                    .records
                    .get(&handle)
                    .is_some_and(|record| record.state == DialogStateV1::Queued)
            {
                return Some(handle);
            }
        }
        None
    }

    fn remove_from_queue(&mut self, handle: &InteractionHandle) {
        let origin = self
            .records
            .get(handle)
            .map(|record| record.owner_service.clone());
        let Some(origin) = origin else {
            return;
        };
        let became_empty = if let Some(queue) = self.queued_by_origin.get_mut(&origin) {
            queue.retain(|candidate| candidate != handle);
            queue.is_empty()
        } else {
            false
        };
        if became_empty {
            self.queued_by_origin.remove(&origin);
            self.origin_turns.retain(|candidate| candidate != &origin);
        }
    }

    fn requeue_generation(&mut self, generation: u64, now_ms: u64, cause: DialogTransitionCause) {
        let handles: Vec<_> = self
            .records
            .values()
            .filter(|record| {
                !record.state.is_terminal()
                    && record
                        .attempt
                        .as_ref()
                        .is_some_and(|attempt| attempt.generation == generation)
            })
            .map(|record| record.handle.clone())
            .collect();
        self.requeue_protected(handles, now_ms, cause);
    }

    fn requeue_protected(
        &mut self,
        handles: Vec<InteractionHandle>,
        now_ms: u64,
        cause: DialogTransitionCause,
    ) {
        let mut handles: Vec<_> = handles
            .into_iter()
            .filter_map(|handle| {
                self.records
                    .get(&handle)
                    .filter(|record| !record.state.is_terminal())
                    .map(|record| (record.created_at_ms, handle))
            })
            .collect();
        handles.sort_by(|left, right| {
            left.0
                .cmp(&right.0)
                .then_with(|| left.1.as_str().cmp(right.1.as_str()))
        });
        handles.dedup_by(|left, right| left.1 == right.1);

        // ctkd's per-dialog catch_unwind is the primary defence. This broker-side
        // budget is the durable backstop against a malicious or repeatedly
        // crashing presenter re-registering the same pre-display poison pill.
        let mut eligible = Vec::with_capacity(handles.len());
        let mut quarantined = Vec::new();
        for (created_at_ms, handle) in handles {
            let record = self.records.get_mut(&handle).expect("record exists");
            record.attempt = None;
            if !record.ever_presented {
                if record.pre_display_requeues >= MAX_DIALOG_PRE_DISPLAY_REQUEUES {
                    quarantined.push(handle);
                    continue;
                }
                record.pre_display_requeues += 1;
            }
            eligible.push((created_at_ms, handle));
        }
        for handle in quarantined {
            self.transition_terminal(
                &handle,
                DialogStateV1::Failed,
                None,
                now_ms,
                DialogTransitionCause::Quarantine,
            );
        }
        let handles = eligible;

        let mut protected_per_origin: HashMap<String, usize> = HashMap::new();
        for (_, handle) in &handles {
            let record = self.records.get(handle).expect("record exists");
            *protected_per_origin
                .entry(record.owner_service.clone())
                .or_default() += 1;
        }

        let protected_over_capacity = handles.len() > self.limits.total
            || protected_per_origin
                .values()
                .any(|count| *count > self.limits.per_origin);
        if protected_over_capacity {
            while let Some(candidate) = self.oldest_queued(None) {
                self.transition_terminal(&candidate, DialogStateV1::Failed, None, now_ms, cause);
            }
        } else {
            for (origin, protected_count) in &protected_per_origin {
                while self
                    .queued_by_origin
                    .get(origin)
                    .map_or(0, VecDeque::len)
                    .saturating_add(*protected_count)
                    > self.limits.per_origin
                {
                    let candidate = self
                        .oldest_queued(Some(origin))
                        .expect("origin overflow requires a queued candidate");
                    self.transition_terminal(
                        &candidate,
                        DialogStateV1::Failed,
                        None,
                        now_ms,
                        cause,
                    );
                }
            }
            while self.queued_len().saturating_add(handles.len()) > self.limits.total {
                let candidate = self
                    .oldest_queued(None)
                    .expect("global overflow requires a queued candidate");
                self.transition_terminal(&candidate, DialogStateV1::Failed, None, now_ms, cause);
            }
        }

        let mut accepted = 0;
        let mut accepted_per_origin: HashMap<String, usize> = HashMap::new();
        for (_, handle) in handles {
            let origin = self
                .records
                .get(&handle)
                .expect("record exists")
                .owner_service
                .clone();
            let origin_count = accepted_per_origin.entry(origin.clone()).or_default();
            if accepted < self.limits.total && *origin_count < self.limits.per_origin {
                let record = self.records.get_mut(&handle).expect("record exists");
                let old_state = record.state;
                record.state = DialogStateV1::Queued;
                accepted += 1;
                *origin_count += 1;
                self.enqueue(&origin, handle.clone());
                self.record_transition(handle, Some(old_state), Some(DialogStateV1::Queued), cause);
            } else {
                self.transition_terminal(&handle, DialogStateV1::Failed, None, now_ms, cause);
            }
        }
    }

    fn oldest_queued(&self, origin: Option<&str>) -> Option<InteractionHandle> {
        self.records
            .values()
            .filter(|record| {
                record.state == DialogStateV1::Queued
                    && origin.is_none_or(|value| record.owner_service == value)
            })
            .min_by(|left, right| {
                left.created_at_ms
                    .cmp(&right.created_at_ms)
                    .then_with(|| left.handle.as_str().cmp(right.handle.as_str()))
            })
            .map(|record| record.handle.clone())
    }

    fn check_lease(&self, candidate: &PresenterLease) -> Result<(), DialogBrokerError> {
        let Some(current) = &self.presenter else {
            return Err(DialogBrokerError::NoPresenter);
        };
        if candidate.instance_epoch != self.instance_epoch || current != candidate {
            return Err(DialogBrokerError::StaleLease);
        }
        Ok(())
    }

    fn check_owner(
        &self,
        service: &str,
        token: &OwnerToken,
        handle: &InteractionHandle,
    ) -> Result<(), DialogBrokerError> {
        let record = self
            .records
            .get(handle)
            .ok_or(DialogBrokerError::NotFound)?;
        // `owner_token` is a caller-held capability secret, so compare it in
        // constant time. It is computed unconditionally (before the `||`), so a
        // service mismatch does not skip the token compare — the timing is
        // token-content-independent regardless of the service. The service name
        // is not secret and stays a plain compare. `subtle`'s slice `ct_eq`
        // short-circuits on differing length (length is not secret) and folds
        // all byte diffs otherwise, so a partial-prefix match is not timeable.
        let token_matches: bool = record
            .owner_token
            .as_str()
            .as_bytes()
            .ct_eq(token.as_str().as_bytes())
            .into();
        if record.owner_service != service || !token_matches {
            return Err(DialogBrokerError::WrongOwner);
        }
        Ok(())
    }

    fn check_current_attempt(
        &self,
        handle: &InteractionHandle,
        generation: u64,
        attempt_token: PresentationAttemptToken,
    ) -> Result<(), DialogBrokerError> {
        let record = self
            .records
            .get(handle)
            .ok_or(DialogBrokerError::NotFound)?;
        let is_current = record.attempt.as_ref().is_some_and(|attempt| {
            attempt.generation == generation && attempt.token == attempt_token
        });
        if !is_current {
            return Err(DialogBrokerError::StaleAttempt);
        }
        Ok(())
    }

    fn mint_presenter_generation(&mut self) -> Result<u64, DialogBrokerError> {
        let generation = self.next_presenter_generation;
        self.next_presenter_generation = generation
            .checked_add(1)
            .ok_or(DialogBrokerError::CounterExhausted)?;
        Ok(generation)
    }

    fn mint_attempt_token(&mut self) -> Result<PresentationAttemptToken, DialogBrokerError> {
        let token = self.next_attempt_token;
        self.next_attempt_token = token
            .checked_add(1)
            .ok_or(DialogBrokerError::CounterExhausted)?;
        Ok(PresentationAttemptToken(token))
    }

    fn expire_handle_if_due(&mut self, handle: &InteractionHandle, now_ms: u64) -> bool {
        let due = self.records.get(handle).is_some_and(|record| {
            !record.state.is_terminal()
                && record
                    .expires_at_ms
                    .is_some_and(|deadline| now_ms >= deadline)
        });
        if due {
            self.transition_terminal(
                handle,
                DialogStateV1::Expired,
                None,
                now_ms,
                DialogTransitionCause::Expire,
            );
        }
        due
    }

    fn transition_terminal(
        &mut self,
        handle: &InteractionHandle,
        state: DialogStateV1,
        value: Option<DialogValueV1>,
        now_ms: u64,
        cause: DialogTransitionCause,
    ) {
        debug_assert!(state.is_terminal());
        if self
            .records
            .get(handle)
            .is_none_or(|record| record.state.is_terminal())
        {
            return;
        }
        self.remove_from_queue(handle);
        let record = self.records.get_mut(handle).expect("record exists");
        let old_state = record.state;
        record.state = state;
        record.value = value;
        record.attempt = None;
        record.terminal_at_ms = Some(now_ms);
        self.terminal_order.push_back(handle.clone());
        if state == DialogStateV1::Expired {
            self.pending_expirations.push_back(handle.clone());
        }
        self.record_transition(handle.clone(), Some(old_state), Some(state), cause);
        self.trim_terminal_count();
    }

    fn trim_terminal_count(&mut self) {
        while self.terminal_order.len() > self.retention.terminal {
            if let Some(handle) = self.terminal_order.pop_front()
                && let Some(record) = self.records.remove(&handle)
            {
                self.record_transition(
                    handle.clone(),
                    Some(record.state),
                    None,
                    DialogTransitionCause::Evict,
                );
                self.pending_count_evictions.push_back(handle);
            }
        }
    }

    fn evict_terminal_ttl(&mut self, now_ms: u64) -> Vec<InteractionHandle> {
        let evicted: Vec<_> = self
            .terminal_order
            .iter()
            .filter(|handle| {
                self.records.get(*handle).is_none_or(|record| {
                    record.terminal_at_ms.is_some_and(|terminal_at| {
                        now_ms.saturating_sub(terminal_at) >= self.retention.terminal_ttl_ms
                    })
                })
            })
            .cloned()
            .collect();
        for handle in &evicted {
            if let Some(record) = self.records.remove(handle) {
                self.record_transition(
                    handle.clone(),
                    Some(record.state),
                    None,
                    DialogTransitionCause::Evict,
                );
            }
        }
        self.terminal_order
            .retain(|handle| !evicted.contains(handle));
        evicted
    }

    fn record_transition(
        &mut self,
        handle: InteractionHandle,
        from: Option<DialogStateV1>,
        to: Option<DialogStateV1>,
        cause: DialogTransitionCause,
    ) {
        if self.pending_transitions.len() < self.transition_limit {
            self.pending_transitions.push(DialogTransition {
                handle,
                from,
                to,
                cause,
            });
        } else {
            self.transitions_overflowed = true;
        }
    }
}

fn initial_progress(request: &DialogRequestV1) -> Option<DialogProgressSnapshot> {
    let DialogRequestV1::Progress {
        common, progress, ..
    } = request
    else {
        return None;
    };
    Some(DialogProgressSnapshot {
        message: common.message.clone(),
        progress: progress.clone(),
    })
}

fn snapshot_from_record(record: &DialogRecord) -> DialogSnapshot {
    DialogSnapshot {
        handle: record.handle.clone(),
        owner_service: record.owner_service.clone(),
        request: record.request.clone(),
        state: record.state,
        created_at_ms: record.created_at_ms,
        expires_at_ms: record.expires_at_ms,
        terminal_at_ms: record.terminal_at_ms,
        progress: record.progress.clone(),
        cancel_requested: record.cancel_requested,
        value: record.value.clone(),
    }
}

#[cfg(test)]
mod tests {
    use cosmix_interaction_schema::{
        DialogCommonV1, DialogProgressValueV1, DialogSeverityV1, MAX_DIALOG_PATH_BYTES,
        MIN_DIALOG_DEADLINE_MS,
    };

    use super::*;
    use crate::RateConfig;

    fn common(title: &str) -> DialogCommonV1 {
        DialogCommonV1 {
            title: title.into(),
            message: Some("body".into()),
            severity: DialogSeverityV1::Info,
        }
    }

    fn message(title: &str) -> DialogOpenRequestV1 {
        DialogOpenRequestV1 {
            dialog: DialogRequestV1::Message {
                common: common(title),
                details: None,
            },
            deadline_ms: None,
        }
    }

    fn progress(cancellable: bool) -> DialogOpenRequestV1 {
        DialogOpenRequestV1 {
            dialog: DialogRequestV1::Progress {
                common: common("Work"),
                progress: DialogProgressValueV1::Determinate {
                    current: 0,
                    total: 10,
                },
                cancellable,
            },
            deadline_ms: None,
        }
    }

    fn owner_token(value: &str) -> OwnerToken {
        OwnerToken(value.into())
    }

    fn handle(value: &str) -> InteractionHandle {
        InteractionHandle(value.into())
    }

    fn roomy_broker(limits: DialogQueueLimits) -> DialogBroker {
        let generous = RateConfig {
            capacity: 1_000.0,
            refill_per_sec: 1_000.0,
        };
        DialogBroker::new(RateLimiter::new_with_global(generous, generous), limits)
    }

    fn roomy_broker_with_retention(
        limits: DialogQueueLimits,
        retention: DialogRetentionPolicy,
    ) -> DialogBroker {
        let generous = RateConfig {
            capacity: 1_000.0,
            refill_per_sec: 1_000.0,
        };
        DialogBroker::new_with_retention(
            RateLimiter::new_with_global(generous, generous),
            limits,
            retention,
        )
    }

    fn open(
        broker: &mut DialogBroker,
        owner: &str,
        id: &str,
        request: DialogOpenRequestV1,
    ) -> (InteractionHandle, OwnerToken) {
        let token = owner_token(&format!("token-{id}"));
        let response = broker
            .open(owner, token.clone(), request, 1_000, handle(id))
            .unwrap();
        (response.handle, token)
    }

    fn present(broker: &mut DialogBroker, lease: &PresenterLease) -> DialogPresentation {
        let presentation = broker.next_presentation(lease, 1_000).unwrap().unwrap();
        broker
            .mark_presented(
                lease,
                &presentation.handle,
                presentation.attempt_token,
                1_000,
            )
            .unwrap();
        presentation
    }

    fn transitions(broker: &mut DialogBroker) -> Vec<DialogTransition> {
        let batch = broker.drain_transitions();
        assert!(!batch.overflowed);
        batch.transitions
    }

    fn presented_with_deadline(
        mut request: DialogOpenRequestV1,
    ) -> (
        DialogBroker,
        InteractionHandle,
        OwnerToken,
        PresenterLease,
        DialogPresentation,
    ) {
        request.deadline_ms = Some(MIN_DIALOG_DEADLINE_MS);
        let mut broker = roomy_broker(DialogQueueLimits::default());
        let token = owner_token("deadline-owner");
        let handle = handle("deadline");
        broker
            .open("owner", token.clone(), request, 0, handle.clone())
            .unwrap();
        let lease = broker.register_presenter("interact-gui", 0).unwrap();
        let presentation = broker.next_presentation(&lease, 0).unwrap().unwrap();
        broker
            .mark_presented(&lease, &handle, presentation.attempt_token, 0)
            .unwrap();
        (broker, handle, token, lease, presentation)
    }

    #[test]
    fn decision_lifecycle_and_owner_result_read() {
        let mut broker = roomy_broker(DialogQueueLimits::default());
        let (handle, token) = open(&mut broker, "mix-script", "d1", message("Question"));
        assert_eq!(
            broker.snapshot(&handle).unwrap().state,
            DialogStateV1::Queued
        );
        let lease = broker.register_presenter("interact-gui", 1_000).unwrap();
        let pending = broker.next_presentation(&lease, 1_000).unwrap().unwrap();
        assert_eq!(
            broker.snapshot(&handle).unwrap().state,
            DialogStateV1::Presenting
        );
        broker
            .mark_presented(&lease, &handle, pending.attempt_token, 1_000)
            .unwrap();
        assert_eq!(
            broker.snapshot(&handle).unwrap().state,
            DialogStateV1::Presented
        );
        broker
            .resolve(
                &lease,
                &handle,
                pending.attempt_token,
                DialogValueV1::Message {},
                1_000,
            )
            .unwrap();
        let result = broker.result("mix-script", &token, &handle).unwrap();
        assert_eq!(result.state, DialogStateV1::Resolved);
        assert_eq!(result.value, Some(DialogValueV1::Message {}));
    }

    #[test]
    fn owner_capability_is_bound_to_service_and_cannot_resolve() {
        let mut broker = roomy_broker(DialogQueueLimits::default());
        let (handle, token) = open(&mut broker, "owner", "d1", message("Question"));
        assert_eq!(
            broker.resolve_as_owner("owner", &token, &handle),
            Err(DialogBrokerError::OwnerCannotResolve)
        );
        assert_eq!(
            broker.resolve_as_owner("other", &token, &handle),
            Err(DialogBrokerError::WrongOwner)
        );
        assert_eq!(
            broker.cancel("owner", &owner_token("wrong"), &handle, 1_000),
            Err(DialogBrokerError::WrongOwner)
        );

        let owner_lease = broker.register_presenter("owner", 1_000).unwrap();
        let presentation = present(&mut broker, &owner_lease);
        assert_eq!(
            broker.resolve(
                &owner_lease,
                &handle,
                presentation.attempt_token,
                DialogValueV1::Message {},
                1_000,
            ),
            Err(DialogBrokerError::OwnerCannotResolve)
        );
    }

    #[test]
    fn double_resolution_is_compare_and_set_rejected() {
        let mut broker = roomy_broker(DialogQueueLimits::default());
        let (handle, _) = open(&mut broker, "owner", "d1", message("Question"));
        let lease = broker.register_presenter("interact-gui", 1_000).unwrap();
        let presentation = present(&mut broker, &lease);
        broker
            .resolve(
                &lease,
                &handle,
                presentation.attempt_token,
                DialogValueV1::Message {},
                1_000,
            )
            .unwrap();
        assert_eq!(
            broker.resolve(
                &lease,
                &handle,
                presentation.attempt_token,
                DialogValueV1::Message {},
                1_000,
            ),
            Err(DialogBrokerError::StaleAttempt)
        );
    }

    #[test]
    fn presenter_replacement_requeues_with_fresh_attempt_and_rejects_stale_cas() {
        let mut broker = roomy_broker(DialogQueueLimits::default());
        let original = message("Frozen");
        let (handle, _) = open(&mut broker, "owner", "d1", original.clone());
        let old_lease = broker.register_presenter("interact-gui", 1_000).unwrap();
        let old = present(&mut broker, &old_lease);

        let new_lease = broker.register_presenter("interact-gui", 1_000).unwrap();
        assert_eq!(
            broker.snapshot(&handle).unwrap().state,
            DialogStateV1::Queued
        );
        assert_eq!(
            broker.resolve(
                &old_lease,
                &handle,
                old.attempt_token,
                DialogValueV1::Message {},
                1_000,
            ),
            Err(DialogBrokerError::StaleLease)
        );
        let fresh = present(&mut broker, &new_lease);
        assert_eq!(fresh.request, original);
        assert_ne!(fresh.attempt_token, old.attempt_token);
        assert_eq!(
            broker.resolve(
                &new_lease,
                &handle,
                old.attempt_token,
                DialogValueV1::Message {},
                1_000,
            ),
            Err(DialogBrokerError::StaleAttempt)
        );
        broker
            .resolve(
                &new_lease,
                &handle,
                fresh.attempt_token,
                DialogValueV1::Message {},
                1_000,
            )
            .unwrap();
    }

    #[test]
    fn release_requeues_current_presentations() {
        let mut broker = roomy_broker(DialogQueueLimits::default());
        let (handle, _) = open(&mut broker, "owner", "d1", message("Question"));
        let lease = broker.register_presenter("interact-gui", 1_000).unwrap();
        present(&mut broker, &lease);
        broker.release_presenter(&lease, 1_000).unwrap();
        assert_eq!(
            broker.snapshot(&handle).unwrap().state,
            DialogStateV1::Queued
        );
        assert_eq!(broker.queued_len(), 1);
    }

    #[test]
    fn progress_update_cancel_request_and_owner_acknowledgement() {
        let mut broker = roomy_broker(DialogQueueLimits::default());
        let (handle, token) = open(&mut broker, "worker", "d1", progress(true));
        let frozen = broker.snapshot(&handle).unwrap().request;
        let lease = broker.register_presenter("interact-gui", 1_000).unwrap();
        let presentation = present(&mut broker, &lease);

        broker
            .update_progress(
                "worker",
                &token,
                &handle,
                DialogProgressPatchV1 {
                    message: Some("Halfway".into()),
                    progress: Some(DialogProgressValueV1::Determinate {
                        current: 5,
                        total: 10,
                    }),
                },
                1_000,
            )
            .unwrap();
        let snapshot = broker.snapshot(&handle).unwrap();
        assert_eq!(snapshot.request, frozen);
        assert_eq!(
            snapshot.progress,
            Some(DialogProgressSnapshot {
                message: Some("Halfway".into()),
                progress: DialogProgressValueV1::Determinate {
                    current: 5,
                    total: 10
                },
            })
        );

        broker
            .request_progress_cancel(&lease, &handle, presentation.attempt_token, 1_000)
            .unwrap();
        assert_eq!(
            broker.snapshot(&handle).unwrap().state,
            DialogStateV1::CancelRequested
        );
        broker
            .complete_progress(
                "worker",
                &token,
                &handle,
                DialogProgressCompletionV1::Cancelled {},
                1_000,
            )
            .unwrap();
        let result = broker.result("worker", &token, &handle).unwrap();
        assert_eq!(result.state, DialogStateV1::Resolved);
        assert_eq!(
            result.value,
            Some(DialogValueV1::Progress {
                completion: DialogProgressCompletionV1::Cancelled {}
            })
        );
    }

    #[test]
    fn progress_cancel_survives_presenter_restart() {
        let mut broker = roomy_broker(DialogQueueLimits::default());
        let (handle, _) = open(&mut broker, "worker", "d1", progress(true));
        let old_lease = broker.register_presenter("interact-gui", 1_000).unwrap();
        let old = present(&mut broker, &old_lease);
        broker
            .request_progress_cancel(&old_lease, &handle, old.attempt_token, 1_000)
            .unwrap();
        let new_lease = broker.register_presenter("interact-gui", 1_000).unwrap();
        let fresh = broker
            .next_presentation(&new_lease, 1_000)
            .unwrap()
            .unwrap();
        assert!(fresh.cancel_requested);
        broker
            .mark_presented(&new_lease, &handle, fresh.attempt_token, 1_000)
            .unwrap();
        assert_eq!(
            broker.snapshot(&handle).unwrap().state,
            DialogStateV1::CancelRequested
        );
    }

    #[test]
    fn progress_guards_kind_cancellability_and_update_validation() {
        let mut broker = roomy_broker(DialogQueueLimits::default());
        let (plain, plain_token) = open(&mut broker, "owner", "plain", message("No"));
        assert_eq!(
            broker.update_progress(
                "owner",
                &plain_token,
                &plain,
                DialogProgressPatchV1 {
                    message: None,
                    progress: None,
                },
                1_000,
            ),
            Err(DialogBrokerError::NotProgress)
        );
        broker.cancel("owner", &plain_token, &plain, 1_000).unwrap();

        let (handle, token) = open(&mut broker, "worker", "progress", progress(false));
        assert!(matches!(
            broker.update_progress(
                "worker",
                &token,
                &handle,
                DialogProgressPatchV1 {
                    message: None,
                    progress: Some(DialogProgressValueV1::Determinate {
                        current: 11,
                        total: 10
                    }),
                },
                1_000,
            ),
            Err(DialogBrokerError::Invalid(_))
        ));
        let lease = broker.register_presenter("interact-gui", 1_000).unwrap();
        let presentation = present(&mut broker, &lease);
        assert_eq!(
            broker.request_progress_cancel(&lease, &handle, presentation.attempt_token, 1_000,),
            Err(DialogBrokerError::NotCancellable)
        );
    }

    #[test]
    fn owner_can_complete_progress_before_presentation() {
        let mut broker = roomy_broker(DialogQueueLimits::default());
        let (handle, token) = open(&mut broker, "worker", "d1", progress(true));
        broker
            .complete_progress(
                "worker",
                &token,
                &handle,
                DialogProgressCompletionV1::Succeeded {},
                1_000,
            )
            .unwrap();
        assert_eq!(broker.queued_len(), 0);
        assert_eq!(
            broker.result("worker", &token, &handle).unwrap().state,
            DialogStateV1::Resolved
        );
    }

    #[test]
    fn injected_deadline_is_duration_and_expires_without_a_clock() {
        let mut broker = roomy_broker(DialogQueueLimits::default());
        let request = DialogOpenRequestV1 {
            deadline_ms: Some(MIN_DIALOG_DEADLINE_MS),
            ..message("Deadline")
        };
        let token = owner_token("token");
        broker
            .open("owner", token.clone(), request, 5_000, handle("d1"))
            .unwrap();
        assert!(broker.expire(5_000 + MIN_DIALOG_DEADLINE_MS - 1).is_empty());
        assert_eq!(
            broker.expire(5_000 + MIN_DIALOG_DEADLINE_MS),
            vec![handle("d1")]
        );
        assert_eq!(broker.queued_len(), 0);
        assert_eq!(
            broker.result("owner", &token, &handle("d1")).unwrap().state,
            DialogStateV1::Expired
        );
    }

    #[test]
    fn deadline_is_checked_by_every_mutating_transition() {
        let due = MIN_DIALOG_DEADLINE_MS;

        let mut presenting = roomy_broker(DialogQueueLimits::default());
        let request = DialogOpenRequestV1 {
            deadline_ms: Some(due),
            ..message("mark")
        };
        presenting
            .open("owner", owner_token("mark"), request, 0, handle("mark"))
            .unwrap();
        let lease = presenting.register_presenter("interact-gui", 0).unwrap();
        let attempt = presenting.next_presentation(&lease, 0).unwrap().unwrap();
        assert_eq!(
            presenting.mark_presented(&lease, &handle("mark"), attempt.attempt_token, due),
            Err(DialogBrokerError::Expired)
        );

        let (mut broker, handle, _, lease, presentation) =
            presented_with_deadline(message("resolve"));
        assert_eq!(
            broker.resolve(
                &lease,
                &handle,
                presentation.attempt_token,
                DialogValueV1::Message {},
                due,
            ),
            Err(DialogBrokerError::Expired)
        );

        let (mut broker, handle, _, lease, presentation) = presented_with_deadline(message("fail"));
        assert_eq!(
            broker.fail_presentation(&lease, &handle, presentation.attempt_token, due),
            Err(DialogBrokerError::Expired)
        );

        let (mut broker, handle, token, _, _) = presented_with_deadline(message("cancel"));
        assert_eq!(
            broker.cancel("owner", &token, &handle, due),
            Err(DialogBrokerError::Expired)
        );

        let (mut broker, handle, _, lease, presentation) = presented_with_deadline(progress(true));
        assert_eq!(
            broker.request_progress_cancel(&lease, &handle, presentation.attempt_token, due,),
            Err(DialogBrokerError::Expired)
        );

        let (mut broker, handle, token, _, _) = presented_with_deadline(progress(true));
        assert_eq!(
            broker.update_progress(
                "owner",
                &token,
                &handle,
                DialogProgressPatchV1 {
                    message: Some("late".into()),
                    progress: None,
                },
                due,
            ),
            Err(DialogBrokerError::Expired)
        );

        let (mut broker, handle, token, _, _) = presented_with_deadline(progress(true));
        assert_eq!(
            broker.complete_progress(
                "owner",
                &token,
                &handle,
                DialogProgressCompletionV1::Succeeded {},
                due,
            ),
            Err(DialogBrokerError::Expired)
        );
    }

    #[test]
    fn presenter_requeue_evicts_oldest_queued_at_capacity() {
        let mut broker = roomy_broker(DialogQueueLimits {
            total: 1,
            per_origin: 1,
        });
        let (active, _) = open(&mut broker, "a", "active", message("active"));
        let old_lease = broker.register_presenter("interact-gui", 1_000).unwrap();
        let presentation = present(&mut broker, &old_lease);
        assert_eq!(presentation.handle, active);

        let (waiting, _) = open(&mut broker, "b", "waiting", message("waiting"));
        let new_lease = broker.register_presenter("interact-gui", 2_000).unwrap();
        assert_ne!(old_lease.generation(), new_lease.generation());
        assert_eq!(
            broker.snapshot(&active).unwrap().state,
            DialogStateV1::Queued
        );
        assert_eq!(
            broker.snapshot(&waiting).unwrap().state,
            DialogStateV1::Failed
        );
        assert_eq!(broker.queued_len(), 1);
        assert_eq!(broker.active_len(), 0);
    }

    #[test]
    fn multi_active_requeue_protects_every_in_flight_record() {
        let mut broker = roomy_broker_with_retention(
            DialogQueueLimits {
                total: 2,
                per_origin: 2,
            },
            DialogRetentionPolicy {
                active: 2,
                ..DialogRetentionPolicy::default()
            },
        );
        let (active_a, _) = open(&mut broker, "a", "active-a", message("active-a"));
        let (active_b, _) = open(&mut broker, "b", "active-b", message("active-b"));
        let old_lease = broker.register_presenter("interact-gui", 1_000).unwrap();
        let attempt_a = present(&mut broker, &old_lease);
        let attempt_b = present(&mut broker, &old_lease);
        assert_eq!(attempt_a.handle, active_a);
        assert_eq!(attempt_b.handle, active_b);

        let (waiting_a, _) = open(&mut broker, "queued", "waiting-a", message("waiting-a"));
        let (waiting_b, _) = open(&mut broker, "queued", "waiting-b", message("waiting-b"));
        let new_lease = broker.register_presenter("interact-gui", 2_000).unwrap();
        assert_ne!(old_lease.generation(), new_lease.generation());

        for handle in [&active_a, &active_b] {
            assert_eq!(
                broker.snapshot(handle).unwrap().state,
                DialogStateV1::Queued
            );
        }
        for handle in [&waiting_a, &waiting_b] {
            assert_eq!(
                broker.snapshot(handle).unwrap().state,
                DialogStateV1::Failed
            );
        }
        assert_eq!(broker.queued_len(), 2);
        assert_eq!(broker.active_len(), 0);
    }

    #[test]
    fn progress_rejects_presenter_terminal_resolution() {
        let mut broker = roomy_broker(DialogQueueLimits::default());
        let (handle, _) = open(&mut broker, "worker", "progress", progress(true));
        let lease = broker.register_presenter("interact-gui", 1_000).unwrap();
        let presentation = present(&mut broker, &lease);
        assert_eq!(
            broker.resolve(
                &lease,
                &handle,
                presentation.attempt_token,
                DialogValueV1::Progress {
                    completion: DialogProgressCompletionV1::Succeeded {},
                },
                1_000,
            ),
            Err(DialogBrokerError::ProgressPresenterResolution)
        );
        assert_eq!(
            broker.snapshot(&handle).unwrap().state,
            DialogStateV1::Queued
        );
        assert_eq!(
            broker.request_progress_cancel(&lease, &handle, presentation.attempt_token, 1_000,),
            Err(DialogBrokerError::StaleAttempt)
        );
        let fresh = present(&mut broker, &lease);
        broker
            .fail_presentation(&lease, &handle, fresh.attempt_token, 1_000)
            .unwrap();
        assert_eq!(
            broker.snapshot(&handle).unwrap().state,
            DialogStateV1::Failed
        );
    }

    #[test]
    fn rejected_resolution_requeues_for_a_fresh_single_use_attempt() {
        let mut broker = roomy_broker(DialogQueueLimits::default());
        let (handle, token) = open(&mut broker, "owner", "d1", message("Question"));
        let old_lease = broker.register_presenter("interact-gui", 1_000).unwrap();
        let stale = present(&mut broker, &old_lease);
        let fresh_lease = broker.register_presenter("interact-gui", 1_000).unwrap();
        assert_ne!(old_lease.generation(), fresh_lease.generation());
        let fresh = present(&mut broker, &fresh_lease);
        assert_ne!(stale.attempt_token, fresh.attempt_token);
        assert_eq!(
            broker.resolve(
                &old_lease,
                &handle,
                stale.attempt_token,
                DialogValueV1::Message {},
                1_000,
            ),
            Err(DialogBrokerError::StaleLease)
        );

        assert!(matches!(
            broker.resolve(
                &fresh_lease,
                &handle,
                fresh.attempt_token,
                DialogValueV1::TextView {},
                1_000,
            ),
            Err(DialogBrokerError::Invalid(_))
        ));
        assert_eq!(
            broker.resolve(
                &fresh_lease,
                &handle,
                fresh.attempt_token,
                DialogValueV1::Message {},
                1_000,
            ),
            Err(DialogBrokerError::StaleAttempt)
        );
        assert_eq!(
            broker.snapshot(&handle).unwrap().state,
            DialogStateV1::Queued
        );

        let accepted = present(&mut broker, &fresh_lease);
        assert_ne!(accepted.attempt_token, fresh.attempt_token);
        broker
            .resolve(
                &fresh_lease,
                &handle,
                accepted.attempt_token,
                DialogValueV1::Message {},
                1_000,
            )
            .unwrap();
        assert_eq!(
            broker.result("owner", &token, &handle).unwrap().state,
            DialogStateV1::Resolved
        );
    }

    #[test]
    fn active_presentations_and_terminal_retention_are_bounded() {
        let retention = DialogRetentionPolicy {
            active: 1,
            terminal: 2,
            terminal_ttl_ms: MIN_DIALOG_DEADLINE_MS,
        };
        let mut broker = roomy_broker_with_retention(
            DialogQueueLimits {
                total: 4,
                per_origin: 4,
            },
            retention,
        );
        open(&mut broker, "owner", "a", message("a"));
        open(&mut broker, "owner", "b", message("b"));
        let lease = broker.register_presenter("interact-gui", 1_000).unwrap();
        let first = broker.next_presentation(&lease, 1_000).unwrap().unwrap();
        assert_eq!(broker.active_len(), 1);
        assert!(broker.next_presentation(&lease, 1_000).unwrap().is_none());
        broker
            .fail_presentation(&lease, &first.handle, first.attempt_token, 1_000)
            .unwrap();

        for (index, id) in ["b", "c", "d"].into_iter().enumerate() {
            if index > 0 {
                open(&mut broker, "owner", id, message(id));
            }
            let presentation = broker.next_presentation(&lease, 2_000).unwrap().unwrap();
            broker
                .fail_presentation(
                    &lease,
                    &presentation.handle,
                    presentation.attempt_token,
                    2_000 + index as u64,
                )
                .unwrap();
        }
        assert_eq!(broker.terminal_len(), 2);
        assert!(broker.snapshot(&handle("a")).is_none());

        let count_outcome = broker.gc(2_002);
        assert_eq!(count_outcome.evicted, vec![handle("b")]);
        assert_eq!(broker.terminal_len(), 2);

        let ttl_outcome = broker.gc(2_002 + MIN_DIALOG_DEADLINE_MS);
        assert_eq!(ttl_outcome.evicted.len(), 2);
        assert_eq!(broker.terminal_len(), 0);
    }

    #[test]
    fn pending_maintenance_reports_are_bounded_without_scheduler_gc() {
        let mut broker = roomy_broker_with_retention(
            DialogQueueLimits {
                total: 1,
                per_origin: 1,
            },
            DialogRetentionPolicy {
                active: 1,
                terminal: 0,
                terminal_ttl_ms: MIN_DIALOG_DEADLINE_MS,
            },
        );
        for index in 0..100 {
            let id = format!("expired-{index}");
            let request = DialogOpenRequestV1 {
                deadline_ms: Some(MIN_DIALOG_DEADLINE_MS),
                ..message(&id)
            };
            let token = owner_token(&id);
            let handle = handle(&id);
            broker
                .open("owner", token.clone(), request, 0, handle.clone())
                .unwrap();
            assert_eq!(
                broker.cancel("owner", &token, &handle, MIN_DIALOG_DEADLINE_MS),
                Err(DialogBrokerError::Expired)
            );
            assert!(broker.pending_expirations.len() <= 1);
            assert!(broker.pending_count_evictions.len() <= 1);
        }
        let final_report = broker.gc(MIN_DIALOG_DEADLINE_MS);
        assert_eq!(final_report.expired.len(), 1);
        assert_eq!(final_report.evicted.len(), 1);
        assert!(broker.pending_expirations.is_empty());
        assert!(broker.pending_count_evictions.is_empty());
    }

    #[test]
    fn gc_expires_idle_dialogs_and_reports_scheduler_work() {
        let mut broker = roomy_broker(DialogQueueLimits::default());
        let request = DialogOpenRequestV1 {
            deadline_ms: Some(MIN_DIALOG_DEADLINE_MS),
            ..message("idle")
        };
        broker
            .open("owner", owner_token("idle"), request, 0, handle("idle"))
            .unwrap();
        let outcome = broker.gc(MIN_DIALOG_DEADLINE_MS);
        assert_eq!(outcome.expired, vec![handle("idle")]);
        assert!(outcome.evicted.is_empty());
    }

    #[test]
    fn oversized_multi_file_result_is_rejected_at_broker_ingestion() {
        let request = DialogOpenRequestV1 {
            dialog: DialogRequestV1::FileOpen {
                common: common("Files"),
                initial_directory: None,
                filters: Vec::new(),
                multiple: true,
            },
            deadline_ms: None,
        };
        let mut broker = roomy_broker(DialogQueueLimits::default());
        let (handle, _) = open(&mut broker, "owner", "files", request);
        let lease = broker.register_presenter("interact-gui", 1_000).unwrap();
        let presentation = present(&mut broker, &lease);
        let value = DialogValueV1::FileOpen {
            paths: (0..5)
                .map(|index| format!("/{index}{}", "x".repeat(MAX_DIALOG_PATH_BYTES - 2)))
                .collect(),
        };
        assert!(matches!(
            broker.resolve(&lease, &handle, presentation.attempt_token, value, 1_000,),
            Err(DialogBrokerError::Invalid(
                DialogValidationError::ResultTooLarge { .. }
            ))
        ));
    }

    #[test]
    fn queue_limits_are_enforced_before_spending_rate_budget() {
        let mut broker = roomy_broker(DialogQueueLimits {
            total: 2,
            per_origin: 1,
        });
        open(&mut broker, "a", "a1", message("a1"));
        assert_eq!(
            broker.open("a", owner_token("a2"), message("a2"), 1_000, handle("a2")),
            Err(DialogBrokerError::OriginQueueFull { max: 1 })
        );
        open(&mut broker, "b", "b1", message("b1"));
        assert_eq!(
            broker.open("c", owner_token("c1"), message("c1"), 1_000, handle("c1")),
            Err(DialogBrokerError::QueueFull { max: 2 })
        );
    }

    #[test]
    fn fair_dequeue_rotates_origins() {
        let mut broker = roomy_broker(DialogQueueLimits {
            total: 8,
            per_origin: 8,
        });
        open(&mut broker, "a", "a1", message("a1"));
        open(&mut broker, "a", "a2", message("a2"));
        open(&mut broker, "b", "b1", message("b1"));
        let lease = broker.register_presenter("interact-gui", 1_000).unwrap();
        let mut order = Vec::new();
        for _ in 0..3 {
            let presentation = broker.next_presentation(&lease, 1_000).unwrap().unwrap();
            order.push(presentation.handle.0.clone());
            broker
                .fail_presentation(
                    &lease,
                    &presentation.handle,
                    presentation.attempt_token,
                    1_000,
                )
                .unwrap();
        }
        assert_eq!(order, ["a1", "b1", "a2"]);
    }

    #[test]
    fn dialog_open_reuses_per_origin_and_global_rate_limiter() {
        let one = RateConfig {
            capacity: 1.0,
            refill_per_sec: 0.0,
        };
        let mut broker = DialogBroker::new(
            RateLimiter::new_with_global(
                one,
                RateConfig {
                    capacity: 2.0,
                    refill_per_sec: 0.0,
                },
            ),
            DialogQueueLimits {
                total: 10,
                per_origin: 10,
            },
        );
        open(&mut broker, "a", "a1", message("a1"));
        assert_eq!(
            broker.open("a", owner_token("a2"), message("a2"), 1_000, handle("a2")),
            Err(DialogBrokerError::RateLimited)
        );
        open(&mut broker, "b", "b1", message("b1"));
        assert_eq!(
            broker.open("c", owner_token("c1"), message("c1"), 1_000, handle("c1")),
            Err(DialogBrokerError::RateLimited)
        );
    }

    #[test]
    fn owner_cancel_and_presenter_failure_are_terminal() {
        let mut broker = roomy_broker(DialogQueueLimits::default());
        let (cancelled, token) = open(&mut broker, "owner", "cancel", message("Cancel"));
        broker.cancel("owner", &token, &cancelled, 1_000).unwrap();
        assert_eq!(
            broker.snapshot(&cancelled).unwrap().state,
            DialogStateV1::Cancelled
        );
        assert_eq!(
            broker.cancel("owner", &token, &cancelled, 1_000),
            Err(DialogBrokerError::AlreadyTerminal)
        );

        let (failed, _) = open(&mut broker, "owner", "failed", message("Fail"));
        let lease = broker.register_presenter("interact-gui", 1_000).unwrap();
        let presentation = present(&mut broker, &lease);
        assert_eq!(presentation.handle, failed);
        broker
            .fail_presentation(&lease, &failed, presentation.attempt_token, 1_000)
            .unwrap();
        assert_eq!(
            broker.snapshot(&failed).unwrap().state,
            DialogStateV1::Failed
        );
    }

    #[test]
    fn invalid_request_and_duplicate_handle_are_rejected() {
        let mut broker = roomy_broker(DialogQueueLimits::default());
        let invalid = DialogOpenRequestV1 {
            dialog: DialogRequestV1::Message {
                common: common(""),
                details: None,
            },
            deadline_ms: None,
        };
        assert!(matches!(
            broker.open("owner", owner_token("token"), invalid, 1_000, handle("d1")),
            Err(DialogBrokerError::Invalid(_))
        ));
        open(&mut broker, "owner", "d1", message("Valid"));
        assert_eq!(
            broker.open(
                "other",
                owner_token("other"),
                message("Other"),
                1_000,
                handle("d1")
            ),
            Err(DialogBrokerError::DuplicateHandle)
        );
    }

    #[test]
    fn presenter_instance_epoch_fences_pre_restart_leases() {
        let mut before_restart = roomy_broker(DialogQueueLimits::default())
            .with_instance_epoch(std::num::NonZeroU64::new(41).unwrap());
        let old_lease = before_restart
            .register_presenter("interact-gui", 1_000)
            .unwrap();
        assert_eq!(old_lease.instance_epoch(), 41);

        let mut after_restart = roomy_broker(DialogQueueLimits::default())
            .with_instance_epoch(std::num::NonZeroU64::new(42).unwrap());
        let fresh_lease = after_restart
            .register_presenter("interact-gui", 1_000)
            .unwrap();
        assert_eq!(fresh_lease.generation(), old_lease.generation());
        assert_eq!(fresh_lease.instance_epoch(), 42);
        assert_eq!(
            after_restart.next_presentation(&old_lease, 1_000),
            Err(DialogBrokerError::StaleLease)
        );
        assert_eq!(
            after_restart.release_presenter(&old_lease, 1_000),
            Err(DialogBrokerError::StaleLease)
        );
    }

    #[test]
    fn explicit_instance_epoch_stamps_the_required_nonzero_value() {
        let epoch = std::num::NonZeroU64::new(7).unwrap();
        let mut broker = roomy_broker(DialogQueueLimits::default()).with_instance_epoch(epoch);
        let lease = broker.register_presenter("interact-gui", 1_000).unwrap();

        assert_eq!(lease.instance_epoch(), epoch.get());
    }

    #[test]
    fn deterministic_epoch_zero_explicitly_provides_no_restart_fencing() {
        let mut before_restart = roomy_broker(DialogQueueLimits::default());
        let old_lease = before_restart
            .register_presenter("interact-gui", 1_000)
            .unwrap();
        let mut after_restart = roomy_broker(DialogQueueLimits::default());
        let fresh_lease = after_restart
            .register_presenter("interact-gui", 1_000)
            .unwrap();

        assert_eq!(old_lease.instance_epoch(), 0);
        assert_eq!(old_lease, fresh_lease);
        assert_eq!(after_restart.next_presentation(&old_lease, 1_000), Ok(None));
    }

    #[test]
    fn next_maintenance_selects_deadline_or_terminal_ttl_whichever_is_earlier() {
        fn broker_with_ttl(ttl_ms: u64) -> DialogBroker {
            roomy_broker_with_retention(
                DialogQueueLimits::default(),
                DialogRetentionPolicy {
                    terminal_ttl_ms: ttl_ms,
                    ..DialogRetentionPolicy::default()
                },
            )
        }

        let mut deadline_first = broker_with_ttl(600_000);
        let mut deadline_request = message("deadline");
        deadline_request.deadline_ms = Some(MIN_DIALOG_DEADLINE_MS);
        deadline_first
            .open(
                "live",
                owner_token("live"),
                deadline_request.clone(),
                0,
                handle("live"),
            )
            .unwrap();
        let terminal_token = owner_token("terminal");
        deadline_first
            .open(
                "done",
                terminal_token.clone(),
                message("done"),
                0,
                handle("done"),
            )
            .unwrap();
        deadline_first
            .cancel("done", &terminal_token, &handle("done"), 1_000)
            .unwrap();
        assert_eq!(
            deadline_first.next_maintenance_at_ms(),
            Some(MIN_DIALOG_DEADLINE_MS)
        );

        let mut ttl_first = broker_with_ttl(50_000);
        ttl_first
            .open(
                "live",
                owner_token("live"),
                deadline_request,
                0,
                handle("live"),
            )
            .unwrap();
        let terminal_token = owner_token("terminal");
        ttl_first
            .open(
                "done",
                terminal_token.clone(),
                message("done"),
                0,
                handle("done"),
            )
            .unwrap();
        ttl_first
            .cancel("done", &terminal_token, &handle("done"), 1_000)
            .unwrap();
        assert_eq!(ttl_first.next_maintenance_at_ms(), Some(51_000));
    }

    #[test]
    fn unrepresentable_terminal_ttl_deadline_is_not_scheduled() {
        let mut broker = roomy_broker_with_retention(
            DialogQueueLimits::default(),
            DialogRetentionPolicy {
                terminal_ttl_ms: 10,
                ..DialogRetentionPolicy::default()
            },
        );
        let dialog = handle("near-max");
        let token = owner_token("near-max");
        broker
            .open(
                "owner",
                token.clone(),
                message("near max"),
                u64::MAX - 1,
                dialog.clone(),
            )
            .unwrap();
        broker
            .cancel("owner", &token, &dialog, u64::MAX - 1)
            .unwrap();

        assert_eq!(broker.next_maintenance_at_ms(), None);
        assert!(broker.gc(u64::MAX).evicted.is_empty());
        assert!(broker.snapshot(&dialog).is_some());
    }

    #[test]
    fn next_presentation_uses_current_progress_message_and_value() {
        let mut broker = roomy_broker(DialogQueueLimits::default());
        let mut request = progress(true);
        let DialogRequestV1::Progress {
            common, progress, ..
        } = &mut request.dialog
        else {
            unreachable!();
        };
        common.message = Some("Starting".into());
        *progress = DialogProgressValueV1::Determinate {
            current: 1,
            total: 10,
        };
        let (dialog, token) = open(&mut broker, "owner", "progress-current", request);
        broker
            .update_progress(
                "owner",
                &token,
                &dialog,
                DialogProgressPatchV1 {
                    message: Some("Uploading".into()),
                    progress: Some(DialogProgressValueV1::Determinate {
                        current: 4,
                        total: 10,
                    }),
                },
                1_000,
            )
            .unwrap();

        let lease = broker.register_presenter("interact-gui", 1_000).unwrap();
        let presentation = broker.next_presentation(&lease, 1_000).unwrap().unwrap();
        assert_eq!(
            presentation.progress,
            Some(DialogProgressSnapshot {
                message: Some("Uploading".into()),
                progress: DialogProgressValueV1::Determinate {
                    current: 4,
                    total: 10,
                },
            })
        );
    }

    #[test]
    fn unchanged_progress_update_is_a_silent_no_op() {
        let mut broker = roomy_broker(DialogQueueLimits::default());
        let (dialog, token) = open(&mut broker, "owner", "no-op", progress(true));
        transitions(&mut broker);

        for patch in [
            DialogProgressPatchV1 {
                message: None,
                progress: None,
            },
            DialogProgressPatchV1 {
                message: Some("body".into()),
                progress: Some(DialogProgressValueV1::Determinate {
                    current: 0,
                    total: 10,
                }),
            },
        ] {
            broker
                .update_progress("owner", &token, &dialog, patch, 1_000)
                .unwrap();
            assert!(transitions(&mut broker).is_empty());
        }
    }

    #[test]
    fn transition_buffer_is_bounded_and_reports_overflow() {
        let mut broker = roomy_broker_with_retention(
            DialogQueueLimits {
                total: 1,
                per_origin: 1,
            },
            DialogRetentionPolicy {
                active: 1,
                terminal: 0,
                terminal_ttl_ms: DEFAULT_DIALOG_TERMINAL_TTL_MS,
            },
        );
        assert_eq!(broker.transition_limit, 6);
        let (dialog, token) = open(&mut broker, "owner", "overflow", progress(true));
        transitions(&mut broker);

        for current in 1..=7 {
            broker
                .update_progress(
                    "owner",
                    &token,
                    &dialog,
                    DialogProgressPatchV1 {
                        message: None,
                        progress: Some(DialogProgressValueV1::Determinate { current, total: 10 }),
                    },
                    1_000,
                )
                .unwrap();
        }
        let batch = broker.drain_transitions();
        assert_eq!(batch.transitions.len(), broker.transition_limit);
        assert!(batch.overflowed);
        assert_eq!(broker.drain_transitions(), DialogTransitionBatch::default());
    }

    #[test]
    fn transition_buffer_reserves_headroom_for_maintain_then_open() {
        let mut broker = roomy_broker_with_retention(
            DialogQueueLimits {
                total: 1,
                per_origin: 1,
            },
            DialogRetentionPolicy {
                active: 1,
                terminal: 0,
                terminal_ttl_ms: DEFAULT_DIALOG_TERMINAL_TTL_MS,
            },
        );
        assert_eq!(broker.transition_limit, 6);
        let deadline_request = DialogOpenRequestV1 {
            deadline_ms: Some(MIN_DIALOG_DEADLINE_MS),
            ..message("deadline")
        };
        broker
            .open(
                "active-owner",
                owner_token("active"),
                deadline_request.clone(),
                0,
                handle("active"),
            )
            .unwrap();
        let lease = broker.register_presenter("interact-gui", 0).unwrap();
        broker.next_presentation(&lease, 0).unwrap().unwrap();
        broker
            .open(
                "queued-owner",
                owner_token("queued"),
                deadline_request,
                0,
                handle("queued"),
            )
            .unwrap();
        transitions(&mut broker);

        broker
            .open(
                "new-owner",
                owner_token("new"),
                message("new"),
                MIN_DIALOG_DEADLINE_MS,
                handle("new"),
            )
            .unwrap();
        let batch = broker.drain_transitions();
        assert!(!batch.overflowed);
        assert_eq!(batch.transitions.len(), 5);
        assert!(batch.transitions.contains(&DialogTransition {
            handle: handle("new"),
            from: None,
            to: Some(DialogStateV1::Queued),
            cause: DialogTransitionCause::Open,
        }));
    }

    #[test]
    fn repeated_pre_display_replacements_quarantine_poison_pill() {
        let mut broker = roomy_broker(DialogQueueLimits::default());
        let (dialog, _) = open(&mut broker, "owner", "poison", message("poison"));
        let mut lease = broker.register_presenter("interact-gui", 1_000).unwrap();
        broker.next_presentation(&lease, 1_000).unwrap().unwrap();
        transitions(&mut broker);

        for generation in 0..MAX_DIALOG_PRE_DISPLAY_REQUEUES {
            lease = broker
                .register_presenter("interact-gui", 2_000 + u64::from(generation))
                .unwrap();
            assert_eq!(
                broker.snapshot(&dialog).unwrap().state,
                DialogStateV1::Queued
            );
            broker.next_presentation(&lease, 2_000).unwrap().unwrap();
            transitions(&mut broker);
        }

        broker.register_presenter("interact-gui", 3_000).unwrap();
        assert_eq!(
            broker.snapshot(&dialog).unwrap().state,
            DialogStateV1::Failed
        );
        assert!(transitions(&mut broker).contains(&DialogTransition {
            handle: dialog,
            from: Some(DialogStateV1::Presenting),
            to: Some(DialogStateV1::Failed),
            cause: DialogTransitionCause::Quarantine,
        }));
    }

    #[test]
    fn repeated_pre_display_releases_quarantine_poison_pill() {
        let mut broker = roomy_broker(DialogQueueLimits::default());
        let (dialog, _) = open(&mut broker, "owner", "release-poison", message("poison"));
        transitions(&mut broker);

        for retry in 0..=MAX_DIALOG_PRE_DISPLAY_REQUEUES {
            let lease = broker
                .register_presenter("interact-gui", 2_000 + u64::from(retry))
                .unwrap();
            broker.next_presentation(&lease, 2_000).unwrap().unwrap();
            broker.release_presenter(&lease, 2_000).unwrap();
            let expected = if retry < MAX_DIALOG_PRE_DISPLAY_REQUEUES {
                DialogStateV1::Queued
            } else {
                DialogStateV1::Failed
            };
            assert_eq!(broker.snapshot(&dialog).unwrap().state, expected);
        }

        assert!(transitions(&mut broker).contains(&DialogTransition {
            handle: dialog,
            from: Some(DialogStateV1::Presenting),
            to: Some(DialogStateV1::Failed),
            cause: DialogTransitionCause::Quarantine,
        }));
    }

    #[test]
    fn repeated_pre_display_resolve_rejections_quarantine_poison_pill() {
        let mut broker = roomy_broker(DialogQueueLimits::default());
        let (dialog, _) = open(&mut broker, "owner", "resolve-poison", message("poison"));
        let lease = broker.register_presenter("interact-gui", 1_000).unwrap();
        transitions(&mut broker);

        for retry in 0..=MAX_DIALOG_PRE_DISPLAY_REQUEUES {
            let presentation = broker.next_presentation(&lease, 2_000).unwrap().unwrap();
            assert_eq!(
                broker.resolve(
                    &lease,
                    &dialog,
                    presentation.attempt_token,
                    DialogValueV1::Message {},
                    2_000,
                ),
                Err(DialogBrokerError::InvalidState(DialogStateV1::Presenting))
            );
            let expected = if retry < MAX_DIALOG_PRE_DISPLAY_REQUEUES {
                DialogStateV1::Queued
            } else {
                DialogStateV1::Failed
            };
            assert_eq!(broker.snapshot(&dialog).unwrap().state, expected);
        }

        assert!(transitions(&mut broker).contains(&DialogTransition {
            handle: dialog,
            from: Some(DialogStateV1::Presenting),
            to: Some(DialogStateV1::Failed),
            cause: DialogTransitionCause::Quarantine,
        }));
    }

    #[test]
    fn previously_displayed_dialog_is_not_quarantined_by_any_requeue_cause() {
        let mut broker = roomy_broker(DialogQueueLimits::default());
        let (dialog, _) = open(&mut broker, "owner", "displayed", message("displayed"));
        let mut lease = broker.register_presenter("interact-gui", 1_000).unwrap();
        let first = broker.next_presentation(&lease, 1_000).unwrap().unwrap();
        broker
            .mark_presented(&lease, &dialog, first.attempt_token, 1_000)
            .unwrap();
        transitions(&mut broker);

        let mut pending = (0..=MAX_DIALOG_PRE_DISPLAY_REQUEUES)
            .map(|retry| {
                broker.release_presenter(&lease, 2_000).unwrap();
                assert_eq!(
                    broker.snapshot(&dialog).unwrap().state,
                    DialogStateV1::Queued
                );
                lease = broker
                    .register_presenter("interact-gui", 2_000 + u64::from(retry))
                    .unwrap();
                broker.next_presentation(&lease, 2_000).unwrap().unwrap()
            })
            .last()
            .unwrap();
        for _ in 0..=MAX_DIALOG_PRE_DISPLAY_REQUEUES {
            assert!(matches!(
                broker.resolve(
                    &lease,
                    &dialog,
                    pending.attempt_token,
                    DialogValueV1::Message {},
                    2_000,
                ),
                Err(DialogBrokerError::InvalidState(DialogStateV1::Presenting))
            ));
            pending = broker.next_presentation(&lease, 2_000).unwrap().unwrap();
        }

        for generation in 0..=MAX_DIALOG_PRE_DISPLAY_REQUEUES {
            lease = broker
                .register_presenter("interact-gui", 3_000 + u64::from(generation))
                .unwrap();
            assert_eq!(
                broker.snapshot(&dialog).unwrap().state,
                DialogStateV1::Queued
            );
            broker.next_presentation(&lease, 2_000).unwrap().unwrap();
            transitions(&mut broker);
        }
        assert_eq!(
            broker.snapshot(&dialog).unwrap().state,
            DialogStateV1::Presenting
        );
        assert_eq!(broker.records.get(&dialog).unwrap().pre_display_requeues, 0);
        assert!(
            transitions(&mut broker)
                .iter()
                .all(|transition| transition.cause != DialogTransitionCause::Quarantine)
        );
    }

    #[test]
    fn transition_drain_reports_open_present_resolve_and_then_empties() {
        let mut broker = roomy_broker(DialogQueueLimits::default());
        let (dialog, _) = open(&mut broker, "owner", "lifecycle", message("Question"));
        assert_eq!(
            transitions(&mut broker),
            vec![DialogTransition {
                handle: dialog.clone(),
                from: None,
                to: Some(DialogStateV1::Queued),
                cause: DialogTransitionCause::Open,
            }]
        );

        let lease = broker.register_presenter("interact-gui", 1_000).unwrap();
        let presentation = broker.next_presentation(&lease, 1_000).unwrap().unwrap();
        assert_eq!(
            transitions(&mut broker),
            vec![DialogTransition {
                handle: dialog.clone(),
                from: Some(DialogStateV1::Queued),
                to: Some(DialogStateV1::Presenting),
                cause: DialogTransitionCause::Present,
            }]
        );

        broker
            .mark_presented(&lease, &dialog, presentation.attempt_token, 1_000)
            .unwrap();
        assert_eq!(
            transitions(&mut broker),
            vec![DialogTransition {
                handle: dialog.clone(),
                from: Some(DialogStateV1::Presenting),
                to: Some(DialogStateV1::Presented),
                cause: DialogTransitionCause::MarkPresented,
            }]
        );

        broker
            .resolve(
                &lease,
                &dialog,
                presentation.attempt_token,
                DialogValueV1::Message {},
                1_000,
            )
            .unwrap();
        assert_eq!(
            transitions(&mut broker),
            vec![DialogTransition {
                handle: dialog,
                from: Some(DialogStateV1::Presented),
                to: Some(DialogStateV1::Resolved),
                cause: DialogTransitionCause::Resolve,
            }]
        );
        assert!(transitions(&mut broker).is_empty());
    }

    #[test]
    fn transition_drain_reports_presenter_failure() {
        let mut broker = roomy_broker(DialogQueueLimits::default());
        let (dialog, _) = open(&mut broker, "owner", "failed", message("Fail"));
        let lease = broker.register_presenter("interact-gui", 1_000).unwrap();
        let presentation = broker.next_presentation(&lease, 1_000).unwrap().unwrap();
        transitions(&mut broker);

        broker
            .fail_presentation(&lease, &dialog, presentation.attempt_token, 1_000)
            .unwrap();
        assert_eq!(
            transitions(&mut broker),
            vec![DialogTransition {
                handle: dialog,
                from: Some(DialogStateV1::Presenting),
                to: Some(DialogStateV1::Failed),
                cause: DialogTransitionCause::Fail,
            }]
        );
    }

    #[test]
    fn replacement_transitions_include_requeue_and_protective_failure() {
        let mut broker = roomy_broker(DialogQueueLimits {
            total: 1,
            per_origin: 1,
        });
        let (active, _) = open(&mut broker, "active-owner", "active", message("active"));
        let old_lease = broker.register_presenter("interact-gui", 1_000).unwrap();
        present(&mut broker, &old_lease);
        let (waiting, _) = open(&mut broker, "waiting-owner", "waiting", message("waiting"));
        transitions(&mut broker);

        broker.register_presenter("interact-gui", 2_000).unwrap();
        let observed = transitions(&mut broker);
        assert!(observed.contains(&DialogTransition {
            handle: active,
            from: Some(DialogStateV1::Presented),
            to: Some(DialogStateV1::Queued),
            cause: DialogTransitionCause::Replace,
        }));
        assert!(observed.contains(&DialogTransition {
            handle: waiting,
            from: Some(DialogStateV1::Queued),
            to: Some(DialogStateV1::Failed),
            cause: DialogTransitionCause::Replace,
        }));
        assert_eq!(observed.len(), 2);
    }

    #[test]
    fn transition_drain_reports_expiry_and_ttl_eviction() {
        let mut broker = roomy_broker_with_retention(
            DialogQueueLimits::default(),
            DialogRetentionPolicy {
                terminal_ttl_ms: 10,
                ..DialogRetentionPolicy::default()
            },
        );
        let mut request = message("expiring");
        request.deadline_ms = Some(MIN_DIALOG_DEADLINE_MS);
        let dialog = handle("expiring");
        broker
            .open("owner", owner_token("owner"), request, 0, dialog.clone())
            .unwrap();
        transitions(&mut broker);

        let outcome = broker.gc(MIN_DIALOG_DEADLINE_MS);
        assert_eq!(outcome.expired, vec![dialog.clone()]);
        assert_eq!(
            transitions(&mut broker),
            vec![DialogTransition {
                handle: dialog.clone(),
                from: Some(DialogStateV1::Queued),
                to: Some(DialogStateV1::Expired),
                cause: DialogTransitionCause::Expire,
            }]
        );

        let outcome = broker.gc(MIN_DIALOG_DEADLINE_MS + 10);
        assert_eq!(outcome.evicted, vec![dialog.clone()]);
        assert_eq!(
            transitions(&mut broker),
            vec![DialogTransition {
                handle: dialog,
                from: Some(DialogStateV1::Expired),
                to: None,
                cause: DialogTransitionCause::Evict,
            }]
        );
        assert!(transitions(&mut broker).is_empty());
    }

    #[test]
    fn withdraw_owner_fails_only_that_owners_live_dialogs() {
        let mut broker = roomy_broker(DialogQueueLimits::default());
        let (active, _) = open(&mut broker, "gone", "a-active", message("active"));
        let lease = broker.register_presenter("interact-gui", 1_000).unwrap();
        present(&mut broker, &lease);
        let (queued, _) = open(&mut broker, "gone", "a-queued", message("queued"));
        let (other, _) = open(&mut broker, "present", "b-queued", message("other"));
        let (terminal, terminal_token) =
            open(&mut broker, "gone", "a-terminal", message("terminal"));
        broker
            .cancel("gone", &terminal_token, &terminal, 1_000)
            .unwrap();
        transitions(&mut broker);

        assert_eq!(
            broker.withdraw_owner("gone", 2_000),
            vec![active.clone(), queued.clone()]
        );
        assert_eq!(
            broker.snapshot(&active).unwrap().state,
            DialogStateV1::Failed
        );
        assert_eq!(
            broker.snapshot(&queued).unwrap().state,
            DialogStateV1::Failed
        );
        assert_eq!(
            broker.snapshot(&other).unwrap().state,
            DialogStateV1::Queued
        );
        assert_eq!(
            broker.snapshot(&terminal).unwrap().state,
            DialogStateV1::Cancelled
        );
        assert_eq!(
            transitions(&mut broker),
            vec![
                DialogTransition {
                    handle: active,
                    from: Some(DialogStateV1::Presented),
                    to: Some(DialogStateV1::Failed),
                    cause: DialogTransitionCause::Withdraw,
                },
                DialogTransition {
                    handle: queued,
                    from: Some(DialogStateV1::Queued),
                    to: Some(DialogStateV1::Failed),
                    cause: DialogTransitionCause::Withdraw,
                },
            ]
        );
    }
}
