//! Wayland drag-and-drop transport and receive state machine for CosMix Desktop.
//!
//! The public API deliberately tracks the capability shape merged by winit
//! PR #4571: opaque `DataTransferId`s, a small `DndAction` enum, entered /
//! positioned / dropped / left events, and destination-pulled lazy data fetch.
//! This crate can be deleted when a released winit provides that capability
//! and Bevy exposes it without losing MIME, action, position, lazy-fetch, or
//! correlated completion information. The trigger is that capability, not a
//! winit version number.
//!
//! This crate contains no Bevy or CTK code. `receive` is the destination-side
//! protocol machine; `send` is the source-side machine and private-MIME echo
//! registry. The two meet only in `transport`, where an own-window echo is
//! admitted as an internally originated receive transfer.
//!
//! # What the consumer must uphold
//!
//! These obligations cannot be enforced from inside this crate, so they are the
//! consumer's contract:
//!
//! * **Re-hit-test and refresh acceptance on every motion and action event, not
//!   only when your idea of the target changes.** Echoing a revision without
//!   re-hit-testing does not make a stale target current. A compositor may
//!   dispatch motion, an action change and the drop itself in a single pump, so
//!   an acceptance built before that pump describes a target and an operation
//!   the pointer has already left behind. Every event carries a
//!   [`types::TransportRevision`]; every [`Acceptance`] must echo the newest one
//!   consumed. A physical drop is held until acceptance is proven to cover the
//!   revisions that preceded it, and fails closed with
//!   [`TerminalReason::DropFenceExpired`] if it never is.
//! * **Call [`WaylandBridge::pump`] regularly on winit's event-loop thread, and
//!   keep winit reading its Wayland socket.** This guest queue deliberately uses
//!   `dispatch_pending` and never competes with winit for socket reads.
//! * **Pass fresh, non-decreasing [`std::time::Instant`] values from one
//!   monotonic clock.** Reusing an old `now` can postpone the crate's bounded
//!   failure paths indefinitely.
//! * **Correlate exactly one `DropComplete` with each delivered drop.** The
//!   completion latch that gates the Wayland `finish` has no other way to know
//!   the application is done.
//! * **For outgoing handoff, construct [`OutgoingPayload`] from real paths,
//!   then call [`WaylandBridge::start_outgoing`] while the original left
//!   button is still held.** The bridge captured that press serial on its own
//!   per-seat `wl_pointer`; it does not implement an arm-time fallback.
//! * **Keep smithay-client-toolkit pinned to exactly 0.19.2 until this bridge is
//!   re-audited against the new source.** Post-leave request routing and callback
//!   ordering deliberately mirror that release rather than an abstract SCTK API.
//!
//! # SCTK 0.19.2 post-drop leave handling
//!
//! KWin normally delivers `drop` followed by `leave`. SCTK correctly retains
//! the offer when it has already dropped, but its `accept` and `set_actions`
//! wrappers suppress requests whenever the pointer has left. The bridge uses
//! the retained proxy directly for final MIME acceptance and the
//! protocol-required final non-Ask `set_actions`; payload `receive` and
//! successful `finish` continue through SCTK. None of those post-drop requests
//! is a protocol deviation. See [`transport`] for the per-request rules and
//! SCTK source references. A later `enter` on the same data device is different:
//! SCTK unconditionally destroys the retained old offer before calling this
//! bridge. That replacement retires the old device correlation and terminates
//! the transfer with [`TerminalReason::OfferReplaced`]. Every request also
//! checks the proxy itself at the point of use; local bridge flags are not a
//! liveness authority.

pub mod icon;
pub mod mime;
pub mod queue;
pub mod receive;
pub mod send;
pub mod transport;
pub mod types;

pub use icon::{OutgoingIcon, OutgoingIconError};
pub use mime::{MimeError, MimeType, decode_payload, encode_uri_list, parse_uri_list};
pub use queue::{
    BoundedEventQueue, EnqueueError, EventClass, QueueConfig, QueueConfigError, QueueEvent,
    QueueStats,
};
pub use receive::{
    AskPhase, ReceiveEffect, ReceiveError, ReceivePhase, ReceiveTransfer, ResourceState,
};
pub use send::{
    EchoCorrelation, NONCE_MIME_PREFIX, NonceLookupError, NonceRegistry, OutgoingEvent,
    OutgoingPayload, OutgoingPayloadError, OutgoingPhase, OutgoingTerminalReason, SendConfig,
    SendConfigError, SendError, TransferNonce, URI_LIST_MIME, UTF8_TEXT_MIME,
};
pub use transport::{BridgeConfig, BridgeConfigError, BridgeError, InitError, WaylandBridge};
pub use types::{
    Acceptance, AcceptanceError, AcceptedContext, ActionMask, BridgeEvent, DataTransferId,
    DeliveryId, DndAction, DndOrigin, DragPayload, DropComplete, DropDecision, DropDecisionKind,
    DropEvent, DropOutcome, Modifiers, PayloadFailure, Position, ProposalRevision, SourceId,
    TargetId, TerminalDisposition, TerminalEvent, TerminalReason, TransportRevision,
};
