//! The `MixerTransport` seam — the typed boundary between the mixer pipeline
//! and whatever carries its control traffic.
//!
//! The trait surface is **semantic** (typed [`WriteRequest`] in, typed
//! [`WriteResponse`] / [`MixerSnapshotResponse`] / [`MeterFrame`] /
//! [`DspApplied`] out) rather than stringly: every codec (JSON bodies, base64
//! A.6 frames) lives INSIDE the transport impl that actually needs it. This is
//! what makes the D4 fused bench arm an honest measurement — an in-process
//! transport pays zero serialization, while the Bus transport keeps paying
//! exactly what it paid before the seam existed.
//!
//! Everything that is *pipeline* semantics — the durable outbox and flusher,
//! gesture baselines and echo-prevention, the revision epoch fence, the
//! awaiting-applied watermark, the latency histograms — lives ABOVE this seam
//! in [`crate::mixer`] and is exercised identically by every transport.
//!
//! Impls: the Bus two-connection worker ([`crate::bus::BusTransport`], `bus`
//! feature) and the fused arm's in-process engine wrapper (out of tree, in
//! `mixer-fused`).

use std::time::Instant;

use bevy::ecs::resource::Resource;
use cosmix_mixer_schema::{
    DspApplied, LeafValue, MeterFrame, MixerSnapshotResponse, WriteRequest, WriteResponse,
};
use serde::Deserialize;

/// Connection state as the mixer pipeline observes it — a transport-agnostic
/// mirror of the Bus bridge's connection lifecycle. An in-process transport is
/// simply `Connected` from its first poll and never leaves.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MixerConnectionState {
    #[default]
    Connecting,
    Connected,
    Disconnected,
    ShuttingDown,
    Fatal,
}

/// One authoritative `(path, value, revision)` change event, with the
/// broker-verified writer identity when the publisher provides one (absent on
/// older daemons and snapshot leaves — own-write attribution then falls back
/// to revisions).
#[derive(Clone, Debug, Deserialize)]
pub struct ChangedEvent {
    pub path: String,
    pub revision: u64,
    pub value: LeafValue,
    #[serde(default)]
    pub source_id: Option<String>,
}

/// A typed reply to one issued request. The variant mirrors which issue method
/// produced the request id — the transport tracks that mapping internally.
#[derive(Clone, Debug)]
pub enum TransportReply {
    /// A decoded bootstrap/refresh snapshot.
    Snapshot(MixerSnapshotResponse),
    /// `Ok` = the decoded write outcome (accepted / rejected / busy);
    /// `Err` = the reply body failed to decode (raw decoder message — the
    /// pipeline prefixes its historical "write response decode: " text).
    Write(Result<WriteResponse, String>),
    /// The live transport clock in seconds (`props.get transport.position`
    /// analogue). A failed or undecodable poll never produces this variant —
    /// the transport surfaces it as a transport-level error instead, which
    /// the pipeline ignores for polls (transient telemetry).
    Position(f64),
}

/// One transport lifecycle or completion event.
#[derive(Clone, Debug)]
pub enum TransportEvent {
    Connection {
        state: MixerConnectionState,
        generation: u64,
    },
    Reply {
        request_id: u64,
        result: Result<TransportReply, String>,
        /// When the transport observed the completion. `None` = "at drain
        /// time" (the Bus worker completes on another thread; the pipeline's
        /// per-frame drain is the observation point — today's behavior).
        /// An in-process transport stamps the instant its synchronous call
        /// returned, so issue→ack measures the call itself, not the frame
        /// cadence. issue→applied stays observe-on-frame for every transport.
        completed_at: Option<Instant>,
    },
    /// Inbound messages were dropped (overflow) — the pipeline must treat any
    /// in-flight snapshot as stale and resync.
    DroppedMessages(usize),
    Fatal(String),
}

/// One inbound telemetry message, stamped with the connection generation it
/// arrived under (the pipeline discards messages from a superseded epoch).
// The Meter variant inlines the 504-byte POD frame. Deliberate: at most one
// meter frame flows per poll (latest-wins), the buffer is reused every frame,
// and boxing would add a heap alloc per 60 Hz frame for no benefit.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug)]
pub enum TransportMessage {
    /// The latest meter frame (latest-wins: at most one per poll).
    Meter { generation: u64, frame: MeterFrame },
    Changed {
        generation: u64,
        event: ChangedEvent,
    },
    Applied {
        generation: u64,
        applied: DspApplied,
    },
    /// A message that failed to decode; `error` is the pre-formatted text the
    /// pipeline surfaces verbatim (kept identical to the pre-seam strings).
    Malformed { generation: u64, error: String },
}

impl TransportMessage {
    /// The connection generation this message was stamped with.
    pub fn generation(&self) -> u64 {
        match self {
            TransportMessage::Meter { generation, .. }
            | TransportMessage::Changed { generation, .. }
            | TransportMessage::Applied { generation, .. }
            | TransportMessage::Malformed { generation, .. } => *generation,
        }
    }
}

/// The per-frame drain buffer. Reused across frames by the pump so a poll
/// allocates nothing in the steady state.
#[derive(Default)]
pub struct TransportPoll {
    pub events: Vec<TransportEvent>,
    pub messages: Vec<TransportMessage>,
}

impl TransportPoll {
    pub fn clear(&mut self) {
        self.events.clear();
        self.messages.clear();
    }
}

/// The transport seam. Issue methods are fire-and-forget (completions arrive
/// via [`poll_events`](Self::poll_events) as [`TransportEvent::Reply`], keyed
/// by the caller-supplied `request_id`); an `Err` return means the request was
/// never issued (queue full / worker gone) and no reply will arrive.
///
/// The per-frame drain is TWO calls, and the split is load-bearing for the
/// latency numbers: the pump drains + reconciles events (recording issue→ack)
/// BEFORE the transport spends anything decoding inbound telemetry in
/// [`poll_messages`](Self::poll_messages) — the pre-seam pump's order, so
/// message decode cost can never inflate a measured ack.
pub trait MixerTransport: Send + Sync + 'static {
    /// The writing identity peers attribute our accepted writes to
    /// (own-write echo discrimination above the seam).
    fn service_name(&self) -> &str;

    /// Issue one revisioned control write.
    fn issue_write(&mut self, request_id: u64, request: &WriteRequest) -> Result<(), String>;

    /// Request the revisioned bootstrap/refresh snapshot.
    fn request_snapshot(&mut self, request_id: u64) -> Result<(), String>;

    /// Request the live transport-clock position (seconds).
    fn request_position(&mut self, request_id: u64) -> Result<(), String>;

    /// Drain every lifecycle/completion event since the last call into `out`
    /// (cleared first). Called once per frame, BEFORE
    /// [`poll_messages`](Self::poll_messages).
    fn poll_events(&mut self, out: &mut Vec<TransportEvent>);

    /// Drain (and decode) every inbound telemetry message since the last call
    /// into `out` (cleared first). Order contract: arrival order with the
    /// ordinary queue capped per frame, then any latest-wins messages
    /// (meters) appended.
    fn poll_messages(&mut self, out: &mut Vec<TransportMessage>);

    /// Discard every queued inbound message — the epoch-recovery purge used
    /// when the pipeline detects a restarted authority.
    fn discard_backlog(&mut self);
}

/// The pipeline's handle to its transport.
#[derive(Resource)]
pub struct MixerTransportRes(pub Box<dyn MixerTransport>);
