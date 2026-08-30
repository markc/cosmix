use std::ops::{BitOr, BitOrAssign};
use std::path::PathBuf;
use std::time::Instant;

/// Opaque identity for one incoming or outgoing OS transfer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DataTransferId(pub u64);

/// Opaque identity for an application drop target.
///
/// Phase 4b may encode a Bevy entity here, but this crate never interprets it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TargetId(pub u64);

/// Opaque identity for the app entity that originated an outgoing drag.
///
/// Phase 5b may encode a Bevy entity here, but this crate never interprets it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SourceId(pub u64);

/// Acceptance freshness within one transfer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProposalRevision(pub u64);

/// Monotonic transport-side revision for motion and action callbacks.
///
/// Phase 4b must echo the newest revision it has consumed when applying an
/// [`Acceptance`]. This is the proof used by the physical-drop fence.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TransportRevision(pub u64);

/// Correlates one delivered drop with its application completion.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DeliveryId(pub u64);

/// Surface-logical position supplied by Wayland.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Position {
    pub x: f64,
    pub y: f64,
}

/// Actions supported by the Wayland data-device protocol.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum DndAction {
    Copy,
    Move,
    #[default]
    Ask,
}

/// A set of accepted actions.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct ActionMask(u8);

impl ActionMask {
    pub const NONE: Self = Self(0);
    pub const COPY: Self = Self(1 << 0);
    pub const MOVE: Self = Self(1 << 1);
    pub const ASK: Self = Self(1 << 2);
    pub const ALL: Self = Self(Self::COPY.0 | Self::MOVE.0 | Self::ASK.0);

    pub const fn contains(self, action: DndAction) -> bool {
        let bit = match action {
            DndAction::Copy => Self::COPY.0,
            DndAction::Move => Self::MOVE.0,
            DndAction::Ask => Self::ASK.0,
        };
        self.0 & bit != 0
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl BitOr for ActionMask {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for ActionMask {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

/// Modifier state frozen with an accepted drop.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Modifiers {
    pub control: bool,
    pub shift: bool,
    pub alt: bool,
    pub super_key: bool,
}

/// Origin of a bridge delivery.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DndOrigin {
    External(DataTransferId),
    Internal(SourceId),
}

/// Payload types shared with CTK's v1 DnD contract.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DragPayload {
    Paths(Vec<PathBuf>),
    Text(String),
}

/// Complete accepted context frozen at `drop_performed`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AcceptedContext {
    pub target: TargetId,
    pub action: DndAction,
    pub modifiers: Modifiers,
    pub origin: DndOrigin,
    pub delivery_id: DeliveryId,
    pub revision: ProposalRevision,
}

/// Application acceptance applied to `wl_data_offer.accept` + `set_actions`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Acceptance {
    pub mime_type: String,
    pub allowed_actions: ActionMask,
    pub preferred: DndAction,
    pub context: AcceptedContext,
    pub observed_transport_revision: TransportRevision,
}

/// Invalid action or freshness combinations rejected at the bridge boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AcceptanceError {
    PreferredNotAllowed {
        preferred: DndAction,
        allowed_actions: ActionMask,
    },
    ContextActionMismatch {
        context_action: DndAction,
        preferred: DndAction,
    },
    FinalActionNotOffered {
        action: DndAction,
        source_actions: ActionMask,
    },
    UnobservedTransportRevision {
        observed: TransportRevision,
        latest_delivered: TransportRevision,
    },
}

impl Acceptance {
    pub fn validate(&self) -> Result<(), AcceptanceError> {
        if !self.allowed_actions.contains(self.preferred) {
            return Err(AcceptanceError::PreferredNotAllowed {
                preferred: self.preferred,
                allowed_actions: self.allowed_actions,
            });
        }
        if self.context.action != self.preferred {
            return Err(AcceptanceError::ContextActionMismatch {
                context_action: self.context.action,
                preferred: self.preferred,
            });
        }
        Ok(())
    }
}

/// Why a payload read failed, mapped one-to-one onto a terminal reason.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PayloadFailure {
    /// The source exceeded the configured payload cap.
    TooLarge,
    /// The source held the pipe open without writing for the inactivity window.
    Inactive,
    /// `read`, `poll`, or the pipe itself failed.
    Pipe,
}

impl PayloadFailure {
    pub fn reason(self) -> TerminalReason {
        match self {
            Self::TooLarge => TerminalReason::PayloadTooLarge,
            Self::Inactive => TerminalReason::PayloadInactivityExpired,
            Self::Pipe => TerminalReason::PipeFailure,
        }
    }
}

/// Canonical delivery produced only after both drop and payload readiness.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DropEvent {
    pub transfer_id: DataTransferId,
    pub target: TargetId,
    pub payload: DragPayload,
    pub action: DndAction,
    pub modifiers: Modifiers,
    pub origin: DndOrigin,
    pub delivery_id: DeliveryId,
    pub accepted_revision: ProposalRevision,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropDecisionKind {
    Copy,
    Move,
    Dismissed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DropDecision {
    pub delivery_id: DeliveryId,
    pub decision: DropDecisionKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropOutcome {
    Completed(DndAction),
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DropComplete {
    pub delivery_id: DeliveryId,
    pub outcome: DropOutcome,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalDisposition {
    Finished,
    Rejected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalReason {
    Completed,
    OfferRejected,
    SourceCancelled,
    SourceFinished,
    PipeFailure,
    WindowTeardown,
    LateWorkerResult,
    LeaveBeforeDrop,
    TargetLost,
    RevisionInvalidated,
    DropFenceExpired,
    AppDismissed,
    AppOperationFailed,
    ActionMismatch,
    AskConfirmationDeadlineExpired,
    PayloadRequestDeadlineExpired,
    PostDecisionDeadlineExpired,
    PostDropFinalActionDeadlineExpired,
    FinalActionRejected,
    PayloadTooLarge,
    PayloadInactivityExpired,
    /// Starting another payload reader would exceed the bridge's derived
    /// concurrent-worker bound.
    PayloadWorkerCapacityExceeded,
    WaylandConnectionLost,
    QueueOverflow,
    /// SCTK destroyed this dropped offer when the same data device entered a
    /// replacement drag.
    OfferReplaced,
    /// The Wayland offer proxy was dead when a request was about to be sent.
    OfferProxyDead,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalEvent {
    pub transfer_id: DataTransferId,
    pub disposition: TerminalDisposition,
    pub reason: TerminalReason,
}

/// Events delivered to Phase 4b after bounded bridge draining.
#[derive(Clone, Debug, PartialEq)]
pub enum BridgeEvent {
    Entered {
        transfer_id: DataTransferId,
        position: Position,
        mime_types: Vec<MimeDescriptor>,
        source_actions: ActionMask,
        transport_revision: TransportRevision,
    },
    Motion {
        transfer_id: DataTransferId,
        position: Position,
        transport_revision: TransportRevision,
    },
    ActionChanged {
        transfer_id: DataTransferId,
        action: Option<DndAction>,
        transport_revision: TransportRevision,
    },
    SourceActionsChanged {
        transfer_id: DataTransferId,
        actions: ActionMask,
        transport_revision: TransportRevision,
    },
    HoverLeft {
        transfer_id: DataTransferId,
        post_drop: bool,
    },
    Drop(DropEvent),
    Terminal(TerminalEvent),
}

/// MIME data kept in events without exposing transport internals.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MimeDescriptor {
    pub essence: String,
    pub raw: String,
}

/// Internal deadline value paired with an `Ask` resolution.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Deadline {
    pub at: Instant,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acceptance(allowed: ActionMask, preferred: DndAction, context: DndAction) -> Acceptance {
        Acceptance {
            mime_type: "text/uri-list".into(),
            allowed_actions: allowed,
            preferred,
            context: AcceptedContext {
                target: TargetId(1),
                action: context,
                modifiers: Modifiers::default(),
                origin: DndOrigin::External(DataTransferId(1)),
                delivery_id: DeliveryId(1),
                revision: ProposalRevision(1),
            },
            observed_transport_revision: TransportRevision(1),
        }
    }

    /// The protocol requires the preferred action to be one of the actions the
    /// destination advertised in the same `set_actions` request.
    #[test]
    fn a_preferred_action_outside_the_allowed_mask_is_rejected() {
        // wayland.xml wl_data_offer.set_actions: preferred_action must be one
        // member of dnd_actions, otherwise invalid_action_mask is raised.
        let acceptance = acceptance(ActionMask::COPY, DndAction::Move, DndAction::Move);
        assert_eq!(
            acceptance.validate(),
            Err(AcceptanceError::PreferredNotAllowed {
                preferred: DndAction::Move,
                allowed_actions: ActionMask::COPY,
            })
        );
    }

    /// Otherwise the compositor negotiates one action while the frozen drop
    /// context reports another — the drop would report an operation that was
    /// never agreed.
    #[test]
    fn a_context_action_disagreeing_with_the_preferred_action_is_rejected() {
        let acceptance = acceptance(ActionMask::ALL, DndAction::Move, DndAction::Copy);
        assert_eq!(
            acceptance.validate(),
            Err(AcceptanceError::ContextActionMismatch {
                context_action: DndAction::Copy,
                preferred: DndAction::Move,
            })
        );
    }

    #[test]
    fn a_consistent_acceptance_validates() {
        assert_eq!(
            acceptance(
                ActionMask::COPY | ActionMask::MOVE,
                DndAction::Move,
                DndAction::Move
            )
            .validate(),
            Ok(())
        );
    }

    #[test]
    fn transport_revisions_order_and_default_to_zero() {
        assert_eq!(TransportRevision::default(), TransportRevision(0));
        assert!(TransportRevision(1) < TransportRevision(2));
    }
}
