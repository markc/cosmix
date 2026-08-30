//! `musicd` `mixer.v1` adapter and the first reusable channel-strip composition.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use bevy::app::{App, Plugin, PreUpdate, Update};
use bevy::ecs::component::Component;
use bevy::ecs::entity::Entity;
use bevy::ecs::event::EntityEvent;
use bevy::ecs::message::{Message, MessageReader, MessageWriter};
use bevy::ecs::observer::On;
use bevy::ecs::query::{Changed, With};
use bevy::ecs::resource::Resource;
use bevy::ecs::schedule::{IntoScheduleConfigs, SystemSet};
use bevy::ecs::system::{Commands, Local, Query, Res, ResMut, SystemParam};
use bevy::picking::Pickable;
use bevy::prelude::{
    default, AlignItems, FlexDirection, JustifyContent, Node, Text, TextFont, UiRect,
};
use bevy::ui::{percent, px};
use cosmix_mixer_schema::{
    from_centi_dbfs, LeafValue, MeterFrame, MixerSnapshotResponse, WriteRequest, WriteResponse,
    FADER_MAX_DB, FADER_MIN_DB, NUM_CHANNELS,
};

use crate::chrome::StatusText;
use crate::latency::LatencyHistogram;

use crate::transport::{
    ChangedEvent, MixerConnectionState, MixerTransport, MixerTransportRes, TransportEvent,
    TransportMessage, TransportPoll, TransportReply,
};

#[cfg(feature = "bus")]
use crate::bus::{BusBridge, BusBridgeConfig, BusBridgePlugin};
use crate::widgets::{
    action_button, fader_sized, hfader_sized, knob_sized, level_meter_sized, toggle_button_sized,
    BusWidget, ControlChange, ControlGestureCancel, ControlMeta, ControlRange, ControlValue,
    MeterLane, MeterValue, NumericControlProps, SetControlValue, SetToggleValue, ValueMapping,
};

pub const METERS_TOPIC: &str = "musicd.mixer.meters";
pub const CHANGED_TOPIC: &str = "musicd.mixer.changed";
pub const APPLIED_TOPIC: &str = "musicd.mixer.applied";
const SNAPSHOT_REFRESH_INTERVAL: Duration = Duration::from_secs(2);

/// Minimum spacing between live-audition streaming writes per path while a
/// gesture is active (~60 Hz cap). Streaming is additionally gated by the
/// per-path in-flight serialization, so the effective rate is
/// `min(frame rate, 1/STREAM_MIN_INTERVAL, ack round-trip)` — throttled
/// latest-wins intent, never per-pixel.
const STREAM_MIN_INTERVAL: Duration = Duration::from_millis(16);

/// Local transport submission failures happen before a write reaches the wire,
/// so they use their own bounded retry budget. Wire-level Busy replies retain
/// the separate retry policy in `retry_busy_writes`.
const LOCAL_ISSUE_MAX_ATTEMPTS: u8 = 4;
const LOCAL_ISSUE_RETRY_BASE: Duration = Duration::from_millis(8);

/// How often the scrubber re-reads the live transport clock via `props.get`.
/// Between polls it extrapolates locally, so this only bounds re-sync drift.
const POSITION_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// The mixer.v1 transport paths the scrubber + transport footer bind to.
/// `pub(crate)`: the Bus transport impl encodes the position-poll body.
pub(crate) const TRANSPORT_POSITION_PATH: &str = "transport.position";
const TRANSPORT_STATE_PATH: &str = "transport.state";
const TRANSPORT_LENGTH_PATH: &str = "transport.length";

/// Scrubber travel used until (or when) `transport.length` is unbounded — the
/// daemon leaves an unbounded (0) length unclamped, so a fixed span keeps the
/// thumb meaningful without pretending to know the end.
const SCRUBBER_FALLBACK_LENGTH_SECS: f32 = 300.0;

#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub enum MixerBindingKind {
    Number,
    Bool,
}

#[derive(Component, Clone, Debug, PartialEq, Eq)]
pub struct MixerBinding {
    pub path: String,
    pub kind: MixerBindingKind,
    /// When set, every commit on this binding writes this fixed enum string
    /// (the control's own `f32` is ignored) — the shape a momentary transport
    /// button needs to write `transport.state = "playing" | "stopped"`.
    pub enum_value: Option<String>,
}

impl MixerBinding {
    pub fn number(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            kind: MixerBindingKind::Number,
            enum_value: None,
        }
    }

    pub fn boolean(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            kind: MixerBindingKind::Bool,
            enum_value: None,
        }
    }

    /// A binding that always commits the fixed enum `value` (used by the Play /
    /// Stop transport buttons, which write `transport.state`).
    pub fn enum_write(path: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            kind: MixerBindingKind::Number,
            enum_value: Some(value.into()),
        }
    }

    fn leaf_value(&self, value: f32) -> LeafValue {
        if let Some(text) = &self.enum_value {
            return LeafValue::Enum(text.clone());
        }
        match self.kind {
            MixerBindingKind::Number => LeafValue::Number(value as f64),
            MixerBindingKind::Bool => LeafValue::Bool(value != 0.0),
        }
    }
}

/// Binds a [`LevelMeter`](crate::widgets::LevelMeter) to one mixer channel's
/// peak feed. Public so app-composed surfaces (arranger lane headers) can
/// carry the same meters the strips do.
#[derive(Component)]
pub struct MixerMeterBinding(pub usize);

#[derive(Component)]
struct MixerName {
    path: String,
    fallback: String,
    /// Compact header names render as explicit word-split lines (each at
    /// most [`NAME_LINE_MAX_CHARS`] chars, centered) instead of glyph wrap.
    split_lines: bool,
}

/// Widest line of a compact strip-header name, in characters.
const NAME_LINE_MAX_CHARS: usize = 6;

/// Split a display name into short centered lines: greedy word packing, each
/// line at most [`NAME_LINE_MAX_CHARS`] chars. A longer word is HARD-TRIMMED
/// to the cap ("Strings" → "String") — an overflowing line would bleed into
/// the neighbouring strip. "Night Run" → "Night\nRun".
fn split_name_lines(name: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    for word in name.split_whitespace() {
        let word: String = word.chars().take(NAME_LINE_MAX_CHARS).collect();
        match lines.last_mut() {
            Some(line)
                if line.chars().count() + 1 + word.chars().count() <= NAME_LINE_MAX_CHARS =>
            {
                line.push(' ');
                line.push_str(&word);
            }
            _ => lines.push(word),
        }
    }
    lines.join("\n")
}

fn render_mixer_name(name: &MixerName, text: &str) -> String {
    if name.split_lines {
        split_name_lines(text)
    } else {
        text.to_string()
    }
}

#[derive(Component)]
struct MixerReadout {
    control: Entity,
    precision: usize,
    suffix: &'static str,
}

#[derive(Resource, Debug)]
pub struct MusicdMixerState {
    pub connection: MixerConnectionState,
    pub ready: bool,
    pub snapshot_revision: Option<u64>,
    pub last_applied_revision: Option<u64>,
    pub real_audio: bool,
    pub audio_fault: bool,
    pub applied_fault: bool,
    pub last_error: Option<String>,
    values: HashMap<String, LeafValue>,
    revisions: HashMap<String, u64>,
    suspected_epoch_revision: Option<u64>,
}

/// Read-only view of the transport state CTK is currently driving towards.
///
/// Unlike [`MusicdMixerState`], which contains only acknowledged store state,
/// this projection also sees CTK's existing per-path command lifecycle. It is
/// intentionally narrow: applications can resolve policy such as Toggle
/// against the effective desired state without gaining access to [`MixerIo`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DesiredTransport {
    pub playing: bool,
    /// True while the value comes from queued, in-flight or retrying command
    /// state rather than the acknowledged mixer store.
    pub provisional: bool,
}

#[derive(SystemParam)]
pub struct TransportState<'w> {
    state: Res<'w, MusicdMixerState>,
    // Optional keeps read-only consumers usable in action/focus harnesses that
    // deliberately omit MusicdMixerPlugin. A production mixer installs it.
    io: Option<Res<'w, MixerIo>>,
}

impl TransportState<'_> {
    /// The effective desired transport state together with whether that value
    /// is still provisional in CTK's command lifecycle.
    pub fn desired(&self) -> Option<DesiredTransport> {
        desired_transport(&self.state, self.io.as_deref())
    }

    /// The effective desired `transport.state`, including queued, in-flight
    /// and retrying writes. `None` means the mixer is not ready or no valid
    /// playing/stopped value is known.
    pub fn desired_playing(&self) -> Option<bool> {
        self.desired().map(|desired| desired.playing)
    }
}

impl Default for MusicdMixerState {
    fn default() -> Self {
        Self {
            connection: MixerConnectionState::Connecting,
            ready: false,
            snapshot_revision: None,
            last_applied_revision: None,
            real_audio: false,
            audio_fault: false,
            applied_fault: false,
            last_error: None,
            values: HashMap::new(),
            revisions: HashMap::new(),
            suspected_epoch_revision: None,
        }
    }
}

impl MusicdMixerState {
    pub fn value(&self, path: &str) -> Option<&LeafValue> {
        self.values.get(path)
    }

    pub fn revision(&self, path: &str) -> Option<u64> {
        self.revisions.get(path).copied()
    }

    fn accept_leaf(&mut self, path: String, value: LeafValue, revision: u64) -> bool {
        if self
            .revision(&path)
            .is_some_and(|current| revision < current)
        {
            return false;
        }
        self.revisions.insert(path.clone(), revision);
        self.values.insert(path, value);
        true
    }

    /// Force a leaf's tracked revision + value to an AUTHORITATIVE reading,
    /// bypassing `accept_leaf`'s monotonic guard. Used ONLY for a CAS
    /// rejection's `current_revision`: the store stating its definitive current
    /// state as of processing our write. That revision can legitimately be
    /// LOWER than our stale tracked one after an authority epoch / song-bank
    /// swap rolls the leaf back — and `transport.position` is deliberately
    /// exempt from the snapshot epoch-reset that would otherwise clear it
    /// (`begin_snapshot`), so without this the monotonic guard would refuse the
    /// rollback forever and every future CAS seek to that leaf would re-reject
    /// (the ruler-drag + load-reset seek wedge).
    fn resync_leaf(&mut self, path: String, value: LeafValue, revision: u64) {
        self.revisions.insert(path.clone(), revision);
        self.values.insert(path, value);
    }

    fn begin_snapshot(&mut self, snapshot: &MixerSnapshotResponse) -> SnapshotDisposition {
        let direct_reset = self
            .snapshot_revision
            .is_some_and(|previous| snapshot.revision < previous);
        let highest_observed = self
            .revisions
            .values()
            .copied()
            .max()
            .into_iter()
            .chain(self.last_applied_revision)
            .max()
            .unwrap_or(0);
        // `transport.position` is exempt: its changed feed is TRANSIENT
        // telemetry stamped with the current GLOBAL store revision (both the
        // daemon and the fused transport publish it that way for the
        // renderers' stale-guards), while a snapshot reports the leaf's own
        // (seek-target write) revision. After any unrelated write those
        // legitimately disagree, and letting the leaf vote here declared a
        // false in-place restart on every snapshot refresh — resetting the
        // extrapolation clock every 2 s (the waves-playhead flicker).
        //
        // Accepted residual: a restart where position is the ONLY regressing
        // leaf (restarted authority + another writer re-advancing the global
        // revision before our refresh) goes undetected. Position's revision is
        // unusable as a restart witness by design; closing this needs an
        // authority epoch/instance id on the wire (already flagged as future
        // work at the daemon's position publisher).
        let path_revision_rollback = snapshot.leaves.iter().any(|leaf| {
            leaf.path != TRANSPORT_POSITION_PATH
                && self
                    .revision(&leaf.path)
                    .is_some_and(|current| leaf.revision < current)
        });
        let lagging_observed_state = snapshot.revision < highest_observed || path_revision_rollback;
        if !direct_reset && lagging_observed_state && self.suspected_epoch_revision.is_none() {
            // A snapshot can legitimately race a newer ack. Require a second
            // lagging snapshot before declaring an in-place authority restart.
            self.suspected_epoch_revision = Some(snapshot.revision);
            return SnapshotDisposition::ConfirmEpoch;
        }
        let epoch_reset =
            direct_reset || (lagging_observed_state && self.suspected_epoch_revision.is_some());
        self.suspected_epoch_revision = None;
        if epoch_reset {
            self.last_applied_revision = None;
        }
        // Bootstrap, reconnect and an authority restart are replacements.
        // Periodic refreshes retain newer per-path revisions so an older
        // snapshot cannot overwrite an acknowledgement that arrived first.
        if !self.ready || epoch_reset {
            self.values.clear();
            self.revisions.clear();
        }
        self.ready = true;
        self.snapshot_revision = Some(snapshot.revision);
        self.real_audio = snapshot.real_audio;
        self.audio_fault = snapshot.audio_fault;
        self.applied_fault = snapshot.applied_fault;
        self.last_error = None;
        if epoch_reset {
            SnapshotDisposition::EpochReset
        } else {
            SnapshotDisposition::Applied
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SnapshotDisposition {
    Applied,
    ConfirmEpoch,
    EpochReset,
}

/// Provenance attached to one CTK mixer command. `surface` is deliberately an
/// opaque label: applications own their richer input-source taxonomy, while CTK
/// only needs enough identity to correlate a command through its write lifecycle.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CommandSource {
    pub surface: Cow<'static, str>,
    pub entity: Option<Entity>,
}

impl CommandSource {
    fn app(surface: &'static str) -> Self {
        Self {
            surface: Cow::Borrowed(surface),
            entity: None,
        }
    }

    fn control(entity: Entity) -> Self {
        Self {
            surface: Cow::Borrowed("ctk:control"),
            entity: Some(entity),
        }
    }

    fn app_entity(surface: &'static str, entity: Entity) -> Self {
        Self {
            surface: Cow::Borrowed(surface),
            entity: Some(entity),
        }
    }
}

/// CTK-local logical-command identity. This is separate from `WriteRequest::op_id`:
/// one logical command may be requeued locally and issued with a fresh wire op.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CommandGeneration(pub u64);

/// Groups the streamed generations emitted by one continuous gesture.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct GestureId(pub u64);

/// The sole gesture allowed to own a mixer path. The owner prevents a second
/// control bound to the same path from adopting or tearing down this lineage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ActiveGesture {
    id: GestureId,
    owner: Entity,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LifecycleReset {
    ConnectionGenerationChanged,
    AuthorityEpochChanged,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LastKnownPhase {
    Desired,
    Issued,
    Acknowledged { revision: u64 },
}

/// Honest terminal outcomes for a logical command. `CoveredByAppliedRevision`
/// intentionally does not claim exact DSP application: musicd coalesces control
/// snapshots and publishes a monotonic applied-revision watermark.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandTerminal {
    Superseded {
        by: CommandGeneration,
    },
    Rejected {
        reason: String,
    },
    Abandoned {
        at: LastKnownPhase,
        reason: LifecycleReset,
    },
    CoveredByAppliedRevision {
        accepted_revision: u64,
        applied_revision: u64,
    },
    CoverageUnknown {
        accepted_revision: u64,
        reason: String,
    },
}

#[derive(Message, Clone, Debug, PartialEq, Eq)]
pub struct TransportCommandOutcome {
    pub path: String,
    pub source: CommandSource,
    pub gesture_id: Option<GestureId>,
    pub generation: CommandGeneration,
    pub terminal: CommandTerminal,
}

#[derive(Clone, Debug)]
struct CommandMeta {
    source: CommandSource,
    gesture_id: Option<GestureId>,
    generation: CommandGeneration,
}

#[derive(Clone, Debug)]
struct QueuedWrite {
    value: LeafValue,
    meta: CommandMeta,
    local_issue_attempts: u8,
    local_issue_due: Option<Instant>,
}

impl QueuedWrite {
    fn new(value: LeafValue, meta: CommandMeta) -> Self {
        Self {
            value,
            meta,
            local_issue_attempts: 0,
            local_issue_due: None,
        }
    }
}

#[derive(Clone, Debug)]
struct IssuedWrite {
    request: WriteRequest,
    meta: CommandMeta,
}

struct AwaitingCoverage {
    path: String,
    accepted_revision: u64,
    issued: Option<Instant>,
    meta: CommandMeta,
}

enum RequestKind {
    Snapshot,
    Write {
        write: IssuedWrite,
        attempt: u8,
    },
    /// A lightweight `props.get transport.position` read (seconds) — the live
    /// clock is transient and its snapshot leaf carries only the seek target, so
    /// the scrubber polls it directly (see [`poll_transport_position`]).
    PositionPoll {
        generation: u64,
        seek_epoch: u64,
        /// `transport.position`'s per-path revision at issue: an EXTERNAL
        /// same-generation seek bumps it, and the pre-seek poll reply must
        /// then be discarded (our own seeks are covered by `seek_epoch`).
        position_revision: Option<u64>,
    },
}

struct RetryWrite {
    due: Instant,
    write: IssuedWrite,
    attempt: u8,
}

struct BufferedChange {
    sync_generation: u64,
    event: ChangedEvent,
}

#[derive(Resource, Default)]
struct MixerIo {
    next_request_id: u64,
    next_op_id: u64,
    next_command_generation: u64,
    pending: HashMap<u64, RequestKind>,
    retries: Vec<RetryWrite>,
    inflight_paths: HashSet<String>,
    active_gestures: HashMap<String, ActiveGesture>,
    queued_latest: HashMap<String, QueuedWrite>,
    last_stream_issue: HashMap<String, Instant>,
    /// Authoritative value captured at gesture start, BEFORE any streamed write
    /// mutates server state — the value a cancelled gesture must return the DSP
    /// and the view to (streaming made "cancel = nothing happened" a write-back
    /// obligation, not just a local restore). Updated mid-gesture by EXTERNAL
    /// authoritative changes (another surface / automation), so cancel yields
    /// to concurrent writers instead of stomping them. The paired revision is
    /// the adoption floor: a delayed OLDER external event must not rewind it.
    gesture_baseline: HashMap<String, (LeafValue, Option<u64>)>,
    /// Revisions our own accepted writes produced while a gesture was active,
    /// per path — the discriminator that keeps our own streamed echoes from
    /// being mistaken for external changes when updating `gesture_baseline`.
    own_write_revisions: HashMap<String, HashSet<u64>>,
    /// P6 instrumentation: oldest-unsent timestamp per queued path (outbox
    /// entry → issue), per-request issue timestamps (issue → reply), and
    /// (revision, issue-time) pairs awaiting their `dsp.applied` (issue → DSP).
    queued_since: HashMap<String, Instant>,
    issued_at: HashMap<u64, Instant>,
    awaiting_applied: Vec<AwaitingCoverage>,
    /// Bumped on every issued `transport.position` write: a position poll
    /// stamped with an older epoch raced a seek and its reply must be
    /// discarded (same-generation provenance the connection generation
    /// cannot express).
    seek_epoch: u64,
    lat_queue: LatencyHistogram,
    lat_rtt: LatencyHistogram,
    lat_applied: LatencyHistogram,
    lat_frame: LatencyHistogram,
    buffered_changes: HashMap<String, BufferedChange>,
    sync_generation: u64,
    last_snapshot_request: Option<Instant>,
    snapshot_refresh_required: bool,
    epoch_fence: bool,
    command_outcomes: Vec<TransportCommandOutcome>,
}

impl MixerIo {
    fn request_id(&mut self) -> u64 {
        self.next_request_id = self.next_request_id.wrapping_add(1).max(1);
        self.next_request_id
    }

    fn command_generation(&mut self) -> CommandGeneration {
        self.next_command_generation = self.next_command_generation.wrapping_add(1).max(1);
        CommandGeneration(self.next_command_generation)
    }

    fn command_meta(
        &mut self,
        source: CommandSource,
        gesture_id: Option<GestureId>,
    ) -> CommandMeta {
        CommandMeta {
            source,
            gesture_id,
            generation: self.command_generation(),
        }
    }

    fn complete(&mut self, path: &str, meta: CommandMeta, terminal: CommandTerminal) {
        self.command_outcomes.push(TransportCommandOutcome {
            path: path.to_string(),
            source: meta.source,
            gesture_id: meta.gesture_id,
            generation: meta.generation,
            terminal,
        });
    }

    /// Insert into the outbox, stamping the OLDEST unsent intent's entry time
    /// (latest-wins on the value, first-wins on the timestamp) for the P6
    /// queue→issue histogram. Intra-gesture stream replacement is expected and
    /// deliberately does not flood the public outcome bus.
    fn queue_write(&mut self, path: &str, queued: QueuedWrite) {
        if let Some(previous) = self.queued_latest.insert(path.to_string(), queued.clone()) {
            if !same_gesture_lineage(&previous.meta, &queued.meta) {
                self.complete(
                    path,
                    previous.meta,
                    CommandTerminal::Superseded {
                        by: queued.meta.generation,
                    },
                );
            }
        }
        self.queued_since
            .entry(path.to_string())
            .or_insert_with(Instant::now);
    }

    /// Terminate every logical write owned by the authority generation being
    /// discarded. Snapshot and position-poll requests carry no command envelope
    /// and are simply purged with the rest of `pending`.
    fn abandon_commands(&mut self, reason: LifecycleReset) {
        let queued = std::mem::take(&mut self.queued_latest);
        for (path, queued) in queued {
            self.complete(
                &path,
                queued.meta,
                CommandTerminal::Abandoned {
                    at: LastKnownPhase::Desired,
                    reason: reason.clone(),
                },
            );
        }

        let retries = std::mem::take(&mut self.retries);
        for retry in retries {
            let path = retry.write.request.path.clone();
            self.complete(
                &path,
                retry.write.meta,
                CommandTerminal::Abandoned {
                    at: LastKnownPhase::Issued,
                    reason: reason.clone(),
                },
            );
        }

        let pending = std::mem::take(&mut self.pending);
        for kind in pending.into_values() {
            if let RequestKind::Write { write, .. } = kind {
                let path = write.request.path.clone();
                self.complete(
                    &path,
                    write.meta,
                    CommandTerminal::Abandoned {
                        at: LastKnownPhase::Issued,
                        reason: reason.clone(),
                    },
                );
            }
        }

        let awaiting = std::mem::take(&mut self.awaiting_applied);
        for awaiting in awaiting {
            self.complete(
                &awaiting.path,
                awaiting.meta,
                CommandTerminal::Abandoned {
                    at: LastKnownPhase::Acknowledged {
                        revision: awaiting.accepted_revision,
                    },
                    reason: reason.clone(),
                },
            );
        }
    }

    fn op_id(&mut self) -> String {
        self.next_op_id = self.next_op_id.wrapping_add(1).max(1);
        format!("ctk-{}-{}", std::process::id(), self.next_op_id)
    }
}

fn same_gesture_lineage(previous: &CommandMeta, next: &CommandMeta) -> bool {
    matches!(
        (previous.gesture_id, next.gesture_id),
        (Some(previous), Some(next)) if previous == next
    )
}

fn publish_command_outcomes(
    mut io: ResMut<MixerIo>,
    mut outcomes: MessageWriter<TransportCommandOutcome>,
) {
    for outcome in io.command_outcomes.drain(..) {
        outcomes.write(outcome);
    }
}

#[derive(Resource, Default)]
struct LatestMeter(Option<MeterFrame>);

/// The live transport clock the scrubber tracks. `base_seconds` is the last
/// value the `props.get` poll returned; between polls the view extrapolates
/// forward by wall-clock while `playing`. `base_at` is `None` until the first
/// poll lands — before that the scrubber is driven by the ~10 Hz changed topic.
#[derive(Resource, Default)]
pub struct TransportPosition {
    base_seconds: f64,
    base_at: Option<Instant>,
    playing: bool,
}

impl TransportPosition {
    /// The live extrapolated transport clock in seconds — the value every
    /// follower (scrubber, readout, piano-roll playhead) renders this frame.
    /// `playing`/`length_seconds` come from the leaf store
    /// ([`MusicdMixerState`]); a non-positive length is unbounded.
    pub fn live_seconds(&self, playing: bool, length_seconds: f64) -> f64 {
        let elapsed = self
            .base_at
            .map(|at| at.elapsed().as_secs_f64())
            .unwrap_or(0.0);
        extrapolate_position_seconds(self.base_seconds, elapsed, playing, length_seconds)
    }

    /// True once an authoritative base (changed event, poll reply, or seek
    /// ack) has landed since the last connection/epoch reset. Followers that
    /// would visually snap on a reset (playheads) should hold their last
    /// position while this is false — the scrubber does the equivalent by
    /// only moving on `Some(base_at)`.
    pub fn has_base(&self) -> bool {
        self.base_at.is_some()
    }
}

/// Marks the horizontal transport scrubber so [`update_transport_position`] can
/// drive its value + domain range as the clock advances and the song loads.
#[derive(Component)]
pub struct TransportScrubber;

/// An app-issued transport seek in seconds (arranger ruler click, keyboard
/// jump). Routed through the SAME write machinery as a scrubber release
/// commit — CAS revision floor, seek-epoch bump (inside `issue_write`),
/// in-flight queueing — so app seeks and scrubber seeks can never fight.
/// Multiple requests in one frame collapse to the last (latest wins).
#[derive(Message, Debug, Clone, Copy)]
pub struct TransportSeekRequest {
    pub seconds: f64,
}

/// Scheduled ingress from application transport messages into CTK's command
/// reducer. Applications that produce those messages in `Update` order their
/// final production/apply set before this set so the command is submitted in
/// the same frame. Reactive control observers and lifecycle servicing are not
/// members of this set.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub struct MixerTransportIngressSystems;

/// Lifecycle phases for a bespoke continuous position surface. Unlike
/// [`TransportSeekRequest`], updates in one gesture share ownership and a
/// gesture envelope through the CTK reducer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TransportSeekGesturePhase {
    Begin { seconds: f64 },
    Update { seconds: f64 },
    Commit { seconds: f64 },
    Cancel,
}

/// Entity-targeted transport gesture submitted directly by a continuous app
/// surface such as Studio's ruler. The owner entity must remain alive until it
/// emits `Commit` or `Cancel`; a producer that rebuilds or despawns the surface
/// must cancel first.
#[derive(EntityEvent, Clone, Copy, Debug, PartialEq)]
pub struct TransportSeekGesture {
    #[event_target]
    pub source: Entity,
    pub phase: TransportSeekGesturePhase,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SubmitMode {
    Stream,
    Commit,
    Discrete,
}

fn on_seek_request(
    mut requests: MessageReader<TransportSeekRequest>,
    mut link: Option<ResMut<MixerTransportRes>>,
    mut state: ResMut<MusicdMixerState>,
    mut io: ResMut<MixerIo>,
) {
    let Some(request) = requests.read().last().copied() else {
        return;
    };
    // Authoritative length straight from the leaf as f64 — the f32 display
    // helper rounds enough to mis-clamp a right-edge seek on long sessions.
    let length = leaf_number(state.value(TRANSPORT_LENGTH_PATH)).unwrap_or(0.0);
    let value = LeafValue::Number(clamp_seek_seconds(request.seconds, length));
    let meta = io.command_meta(CommandSource::app("ctk:app-seek"), None);
    submit_write(
        link.as_deref_mut(),
        &mut state,
        &mut io,
        TRANSPORT_POSITION_PATH.to_string(),
        QueuedWrite::new(value, meta),
        SubmitMode::Discrete,
    );
}

fn transport_live_position(transport: &TransportPosition, state: &MusicdMixerState) -> Option<f64> {
    transport.base_at.map(|at| {
        extrapolate_position_seconds(
            transport.base_seconds,
            at.elapsed().as_secs_f64(),
            transport_is_playing(state),
            transport_length_secs(state) as f64,
        )
    })
}

fn reject_transport_seek_gesture(
    state: &mut MusicdMixerState,
    io: &mut MixerIo,
    source: Entity,
    reason: &'static str,
) {
    let reason = reason.to_string();
    state.last_error = Some(reason.clone());
    let meta = io.command_meta(
        CommandSource::app_entity("ctk:transport-seek-gesture", source),
        None,
    );
    io.complete(
        TRANSPORT_POSITION_PATH,
        meta,
        CommandTerminal::Rejected { reason },
    );
}

fn on_transport_seek_gesture(
    gesture: On<TransportSeekGesture>,
    mut link: Option<ResMut<MixerTransportRes>>,
    mut state: ResMut<MusicdMixerState>,
    mut io: ResMut<MixerIo>,
    transport: Res<TransportPosition>,
) {
    let source = gesture.source;
    let command_source = || CommandSource::app_entity("ctk:transport-seek-gesture", source);
    let length = leaf_number(state.value(TRANSPORT_LENGTH_PATH)).unwrap_or(0.0);

    match gesture.phase {
        TransportSeekGesturePhase::Begin { seconds } => {
            let existing = io.active_gestures.get(TRANSPORT_POSITION_PATH).copied();
            if existing.is_some_and(|active| active.owner != source) {
                reject_transport_seek_gesture(
                    &mut state,
                    &mut io,
                    source,
                    "transport position gesture is owned by another entity",
                );
                return;
            }

            let generation = io.command_generation();
            let (gesture_id, acquired) = match existing {
                Some(active) => (active.id, false),
                None => {
                    let active = ActiveGesture {
                        id: GestureId(generation.0),
                        owner: source,
                    };
                    io.active_gestures
                        .insert(TRANSPORT_POSITION_PATH.to_string(), active);
                    (active.id, true)
                }
            };
            if !io.gesture_baseline.contains_key(TRANSPORT_POSITION_PATH) {
                let live_position = transport_live_position(&transport, &state);
                if let Some(baseline) =
                    seed_gesture_baseline(&io, &state, TRANSPORT_POSITION_PATH, live_position)
                {
                    io.gesture_baseline
                        .insert(TRANSPORT_POSITION_PATH.to_string(), baseline);
                }
            }
            let meta = CommandMeta {
                source: command_source(),
                gesture_id: Some(gesture_id),
                generation,
            };
            let (_, accepted) = submit_write(
                link.as_deref_mut(),
                &mut state,
                &mut io,
                TRANSPORT_POSITION_PATH.to_string(),
                QueuedWrite::new(LeafValue::Number(clamp_seek_seconds(seconds, length)), meta),
                SubmitMode::Stream,
            );
            if acquired && !accepted {
                io.active_gestures.remove(TRANSPORT_POSITION_PATH);
                io.gesture_baseline.remove(TRANSPORT_POSITION_PATH);
                io.own_write_revisions.remove(TRANSPORT_POSITION_PATH);
            }
        }
        TransportSeekGesturePhase::Update { seconds } => {
            let Some(active) = io
                .active_gestures
                .get(TRANSPORT_POSITION_PATH)
                .copied()
                .filter(|active| active.owner == source)
            else {
                reject_transport_seek_gesture(
                    &mut state,
                    &mut io,
                    source,
                    "transport position gesture update has no matching owner",
                );
                return;
            };
            let meta = io.command_meta(command_source(), Some(active.id));
            submit_write(
                link.as_deref_mut(),
                &mut state,
                &mut io,
                TRANSPORT_POSITION_PATH.to_string(),
                QueuedWrite::new(LeafValue::Number(clamp_seek_seconds(seconds, length)), meta),
                SubmitMode::Stream,
            );
        }
        TransportSeekGesturePhase::Commit { seconds } => {
            let Some(active) = io
                .active_gestures
                .get(TRANSPORT_POSITION_PATH)
                .copied()
                .filter(|active| active.owner == source)
            else {
                reject_transport_seek_gesture(
                    &mut state,
                    &mut io,
                    source,
                    "transport position gesture commit has no matching owner",
                );
                return;
            };
            io.active_gestures.remove(TRANSPORT_POSITION_PATH);
            io.gesture_baseline.remove(TRANSPORT_POSITION_PATH);
            io.own_write_revisions.remove(TRANSPORT_POSITION_PATH);
            let meta = io.command_meta(command_source(), Some(active.id));
            submit_write(
                link.as_deref_mut(),
                &mut state,
                &mut io,
                TRANSPORT_POSITION_PATH.to_string(),
                QueuedWrite::new(LeafValue::Number(clamp_seek_seconds(seconds, length)), meta),
                SubmitMode::Commit,
            );
        }
        TransportSeekGesturePhase::Cancel => {
            let Some(active) = io
                .active_gestures
                .get(TRANSPORT_POSITION_PATH)
                .copied()
                .filter(|active| active.owner == source)
            else {
                reject_transport_seek_gesture(
                    &mut state,
                    &mut io,
                    source,
                    "transport position gesture cancel has no matching owner",
                );
                return;
            };
            let (cancel_meta, baseline) =
                take_cancelled_gesture(&mut io, TRANSPORT_POSITION_PATH, active, command_source());
            if let Some(baseline) = baseline {
                submit_write(
                    link.as_deref_mut(),
                    &mut state,
                    &mut io,
                    TRANSPORT_POSITION_PATH.to_string(),
                    QueuedWrite::new(baseline, cancel_meta),
                    SubmitMode::Commit,
                );
            }
        }
    }
}

/// Marks the `M:SS / M:SS` transport time readout text.
#[derive(Component)]
pub struct TransportTimeReadout;

/// Where the pipeline's transport comes from: the Bus bridge worker (the
/// split arm), or a caller-supplied [`MixerTransport`] (the fused bench arm,
/// tests). The custom box is `take()`n at build time.
enum TransportSource {
    #[cfg(feature = "bus")]
    Bus {
        noded_url: String,
    },
    Custom(std::sync::Mutex<Option<Box<dyn MixerTransport>>>),
}

pub struct MusicdMixerPlugin {
    source: TransportSource,
    #[cfg_attr(not(feature = "bus"), allow(dead_code))]
    service_name: String,
}

impl MusicdMixerPlugin {
    #[cfg(feature = "bus")]
    pub fn new(noded_url: impl Into<String>) -> Self {
        Self {
            source: TransportSource::Bus {
                noded_url: noded_url.into(),
            },
            // <view>-<engine>-<instance> per the 2026-07-17 view/engine
            // naming decision: the view is what a caller asks for, the
            // engine is the bake-off arm, the pid disambiguates instances.
            service_name: format!("mixer-bevy-{}", std::process::id()),
        }
    }

    #[cfg(feature = "bus")]
    pub fn with_service_name(mut self, service_name: impl Into<String>) -> Self {
        self.service_name = service_name.into();
        self
    }

    /// Run the identical pipeline over a caller-supplied transport (the D4
    /// fused arm swaps in its in-process engine here; the board, the systems,
    /// and every pipeline semantic are shared verbatim).
    pub fn with_transport(transport: Box<dyn MixerTransport>) -> Self {
        let service_name = transport.service_name().to_string();
        Self {
            source: TransportSource::Custom(std::sync::Mutex::new(Some(transport))),
            service_name,
        }
    }
}

impl Plugin for MusicdMixerPlugin {
    fn build(&self, app: &mut App) {
        match &self.source {
            #[cfg(feature = "bus")]
            TransportSource::Bus { noded_url } => {
                let mut config = BusBridgeConfig::new(&self.service_name, noded_url);
                config.subscriptions = vec![
                    METERS_TOPIC.into(),
                    CHANGED_TOPIC.into(),
                    APPLIED_TOPIC.into(),
                ];
                config.latest_topics = vec![METERS_TOPIC.into()];
                config.max_messages_per_frame = 128;
                app.add_plugins(BusBridgePlugin::new(config));
                app.add_systems(
                    bevy::app::PreStartup,
                    install_bus_transport.after(crate::bus::start_bridge),
                );
            }
            TransportSource::Custom(slot) => {
                let transport = slot
                    .lock()
                    .unwrap()
                    .take()
                    .expect("MusicdMixerPlugin::with_transport built twice");
                app.insert_resource(MixerTransportRes(transport));
            }
        }

        app.init_resource::<MusicdMixerState>()
            .init_resource::<MixerIo>()
            .init_resource::<LatestMeter>()
            .init_resource::<TransportPosition>()
            .add_message::<TransportSeekRequest>()
            .add_message::<TransportCommandOutcome>()
            .add_observer(on_control_change)
            .add_observer(on_control_gesture_cancel)
            .add_observer(on_transport_seek_gesture)
            .add_systems(PreUpdate, pump_transport)
            .add_systems(Update, on_seek_request.in_set(MixerTransportIngressSystems))
            // Last, not Update: structures spawned (and command-flushed)
            // anywhere in Update are seeded THIS frame, strictly before the
            // next frame's PreUpdate picking can deliver a click to a
            // still-default widget.
            .add_systems(
                bevy::app::Last,
                (seed_added_bindings, publish_command_outcomes).chain(),
            )
            .add_systems(
                Update,
                (
                    // Smoke gestures fire BEFORE the flusher so a queued
                    // streaming write issues in the same frame (matching a
                    // real pointer gesture, whose ControlChange lands in
                    // PreUpdate picking — an end-of-chain smoke would pay a
                    // one-frame queue→issue penalty by construction).
                    smoke::smoke_write,
                    smoke::smoke_stream,
                    retry_busy_writes,
                    flush_queued_writes,
                    apply_latest_meter,
                    update_readouts,
                    update_status_text,
                    poll_transport_position,
                    update_transport_position,
                    report_latency,
                )
                    .chain(),
            );
    }
}

/// Wrap the freshly-started Bus bridge in the transport seam. Ordered after
/// [`crate::bus::start_bridge`] (the auto sync point applies its Commands
/// first, so the bridge resource exists here).
#[cfg(feature = "bus")]
fn install_bus_transport(mut commands: Commands, mut bridge: ResMut<BusBridge>) {
    commands.insert_resource(MixerTransportRes(Box::new(bridge.mixer_transport())));
}

/// Release one verified gesture owner and discard any queued/Busy-retrying
/// stream it owned. The returned baseline is the compensation value callers
/// must submit as a commit; an already wire-in-flight write remains serialized
/// ahead of that compensation by the existing path machinery.
fn take_cancelled_gesture(
    io: &mut MixerIo,
    path: &str,
    active: ActiveGesture,
    source: CommandSource,
) -> (CommandMeta, Option<LeafValue>) {
    io.active_gestures.remove(path);
    io.own_write_revisions.remove(path);
    let cancel_meta = io.command_meta(source, Some(active.id));
    // A cancelled gesture's queued streaming intent must not be committed by a
    // later drain, and a Busy-retrying stream must not resurrect it either.
    if let Some(previous) = io.queued_latest.remove(path) {
        if !same_gesture_lineage(&previous.meta, &cancel_meta) {
            io.complete(
                path,
                previous.meta,
                CommandTerminal::Superseded {
                    by: cancel_meta.generation,
                },
            );
        }
    }
    io.queued_since.remove(path);
    let retries = std::mem::take(&mut io.retries);
    for retry in retries {
        if retry.write.request.path == path {
            if !same_gesture_lineage(&retry.write.meta, &cancel_meta) {
                io.complete(
                    path,
                    retry.write.meta,
                    CommandTerminal::Superseded {
                        by: cancel_meta.generation,
                    },
                );
            }
        } else {
            io.retries.push(retry);
        }
    }
    // A purged Busy retry leaves no wire request behind its in-flight marker —
    // clear the phantom or the baseline below queues behind nothing forever.
    let wire_inflight = io
        .pending
        .values()
        .any(|kind| matches!(kind, RequestKind::Write { write, .. } if write.request.path == path));
    if !wire_inflight {
        io.inflight_paths.remove(path);
    }
    let baseline = io
        .gesture_baseline
        .remove(path)
        .map(|(value, _floor)| value);
    (cancel_meta, baseline)
}

fn on_control_gesture_cancel(
    cancel: On<ControlGestureCancel>,
    mut transport: Option<ResMut<MixerTransportRes>>,
    bindings: Query<&MixerBinding>,
    mut state: ResMut<MusicdMixerState>,
    mut io: ResMut<MixerIo>,
    mut commands: Commands,
) {
    let Ok(binding) = bindings.get(cancel.source) else {
        return;
    };
    let Some(active) = io.active_gestures.get(&binding.path).copied() else {
        restore_authoritative(&binding.path, cancel.source, &state, &mut commands);
        return;
    };
    if active.owner != cancel.source {
        // A competing control on the same binding never owns the path gesture,
        // so its cancellation must not purge the incumbent's stream/baseline.
        restore_authoritative(&binding.path, cancel.source, &state, &mut commands);
        return;
    }
    let (cancel_meta, baseline) = take_cancelled_gesture(
        &mut io,
        &binding.path,
        active,
        CommandSource::control(cancel.source),
    );
    // Streaming means the DSP may already sit on a mid-drag value: cancel is a
    // write-back to the pre-gesture baseline, not just a local view restore.
    if let Some(baseline) = baseline {
        set_control_from_value(cancel.source, &baseline, &mut commands);
        submit_write(
            transport.as_deref_mut(),
            &mut state,
            &mut io,
            binding.path.clone(),
            QueuedWrite::new(baseline, cancel_meta),
            SubmitMode::Commit,
        );
        return;
    }
    restore_authoritative(&binding.path, cancel.source, &state, &mut commands);
}

fn on_control_change(
    change: On<ControlChange>,
    mut link: Option<ResMut<MixerTransportRes>>,
    bindings: Query<&MixerBinding>,
    mut state: ResMut<MusicdMixerState>,
    mut io: ResMut<MixerIo>,
    transport: Res<TransportPosition>,
    mut commands: Commands,
) {
    let Ok(binding) = bindings.get(change.source) else {
        return;
    };
    if !change.is_final {
        let generation = io.command_generation();
        let (gesture_id, owns_path) = match io.active_gestures.get(&binding.path).copied() {
            Some(active) if active.owner == change.source => (active.id, true),
            Some(_) => {
                // A second control bound to the same path is an independent
                // logical command. It gets its own lineage but does not become
                // a concurrent path gesture or touch the incumbent baseline.
                (GestureId(generation.0), false)
            }
            None => {
                let active = ActiveGesture {
                    id: GestureId(generation.0),
                    owner: change.source,
                };
                io.active_gestures.insert(binding.path.clone(), active);
                (active.id, true)
            }
        };
        // Live-audition stream: queue the latest intermediate value so
        // `flush_stream_writes` can forward it (throttled, latest-wins) and the
        // DSP follows the hand mid-drag. Errors stay silent here — the release
        // commit is the write that reports.
        // Capture the pre-gesture authoritative value once, before any
        // streamed ack rewrites server state — cancel restores to this.
        if state.connection == MixerConnectionState::Connected
            && state.ready
            && owns_path
            && !io.gesture_baseline.contains_key(&binding.path)
        {
            let live_position = transport_live_position(&transport, &state);
            if let Some(baseline) = seed_gesture_baseline(&io, &state, &binding.path, live_position)
            {
                io.gesture_baseline.insert(binding.path.clone(), baseline);
            }
        }
        let value = binding.leaf_value(change.value);
        submit_write(
            link.as_deref_mut(),
            &mut state,
            &mut io,
            binding.path.clone(),
            QueuedWrite::new(
                value,
                CommandMeta {
                    source: CommandSource::control(change.source),
                    gesture_id: Some(gesture_id),
                    generation,
                },
            ),
            if owns_path {
                SubmitMode::Stream
            } else {
                SubmitMode::Discrete
            },
        );
        return;
    }
    let gesture_id = io
        .active_gestures
        .get(&binding.path)
        .filter(|active| active.owner == change.source)
        .map(|active| active.id);
    if gesture_id.is_some() {
        io.active_gestures.remove(&binding.path);
        io.gesture_baseline.remove(&binding.path);
        io.own_write_revisions.remove(&binding.path);
    }
    let value = binding.leaf_value(change.value);
    let meta = io.command_meta(CommandSource::control(change.source), gesture_id);
    let mode = if gesture_id.is_some() {
        SubmitMode::Commit
    } else {
        SubmitMode::Discrete
    };
    let (_, accepted) = submit_write(
        link.as_deref_mut(),
        &mut state,
        &mut io,
        binding.path.clone(),
        QueuedWrite::new(value, meta),
        mode,
    );
    if !accepted {
        restore_authoritative(&binding.path, change.source, &state, &mut commands);
    }
}

/// P6 latency report: one summary line every 5s, and only when new samples
/// arrived since the previous line. Also records the frame-time histogram.
fn report_latency(
    time: Res<bevy::time::Time>,
    mut io: ResMut<MixerIo>,
    mut last_print: Local<Option<(Instant, u64)>>,
) {
    io.lat_frame.record(time.delta());
    let total =
        io.lat_queue.count() + io.lat_rtt.count() + io.lat_applied.count() + io.lat_frame.count();
    // First frame initialises the interval without printing.
    let Some((at, seen)) = *last_print else {
        *last_print = Some((Instant::now(), total));
        return;
    };
    if at.elapsed() < Duration::from_secs(5) {
        return;
    }
    let changed = seen != total;
    if changed && total > 0 {
        println!(
            "ctk-latency queue→issue[{}] issue→ack[{}] issue→applied[{}] frame[{}]",
            io.lat_queue.summary(),
            io.lat_rtt.summary(),
            io.lat_applied.summary(),
            io.lat_frame.summary(),
        );
    }
    *last_print = Some((Instant::now(), total));
}

/// True when `path` may issue its next live-audition streaming write (per-path
/// `STREAM_MIN_INTERVAL` spacing; a never-streamed path is immediately due).
fn stream_due(io: &MixerIo, path: &str) -> bool {
    io.last_stream_issue
        .get(path)
        .is_none_or(|last| last.elapsed() >= STREAM_MIN_INTERVAL)
}

fn local_issue_retry_delay(attempts: u8) -> Duration {
    let exponent = u32::from(attempts.saturating_sub(1));
    LOCAL_ISSUE_RETRY_BASE.saturating_mul(1_u32 << exponent)
}

/// A local transport submission error means no request reached the wire. Keep
/// the original command envelope and queue timestamp, but bound retries so a
/// permanently broken local transport produces one observable terminal result.
fn handle_local_issue_failure(
    io: &mut MixerIo,
    path: &str,
    mut command: QueuedWrite,
    entered: Option<Instant>,
    error: String,
) {
    command.local_issue_attempts = command.local_issue_attempts.saturating_add(1);
    if command.local_issue_attempts >= LOCAL_ISSUE_MAX_ATTEMPTS {
        let attempts = command.local_issue_attempts;
        io.complete(
            path,
            command.meta,
            CommandTerminal::Rejected {
                reason: format!("local transport issue failed after {attempts} attempts: {error}"),
            },
        );
        return;
    }

    command.local_issue_due =
        Some(Instant::now() + local_issue_retry_delay(command.local_issue_attempts));
    io.queued_latest.insert(path.to_string(), command);
    io.queued_since
        .insert(path.to_string(), entered.unwrap_or_else(Instant::now));
}

/// Drain the `queued_latest` outbox. Two classes share it: mid-gesture paths
/// stream throttled latest-wins live-audition writes (spaced by
/// [`STREAM_MIN_INTERVAL`]); non-gesture paths (a queued release, a cancel
/// compensation, a value stranded by a transport error) flush immediately —
/// the outbox is durable intent, not best-effort. Runs every frame; never
/// overlaps an in-flight write. Local issue errors retain the command envelope
/// under bounded exponential backoff; a real disconnect clears the outbox via
/// the resync path.
fn flush_queued_writes(
    mut link: ResMut<MixerTransportRes>,
    state: Res<MusicdMixerState>,
    mut io: ResMut<MixerIo>,
    bindings: Query<(Entity, &MixerBinding)>,
    mut commands: Commands,
) {
    if state.connection != MixerConnectionState::Connected || !state.ready {
        return;
    }
    let now = Instant::now();
    let due: Vec<String> = io
        .queued_latest
        .keys()
        .filter(|path| !io.inflight_paths.contains(*path))
        .filter(|path| {
            io.queued_latest
                .get(*path)
                .is_some_and(|command| command.local_issue_due.is_none_or(|due| due <= now))
        })
        .filter(|path| !io.active_gestures.contains_key(*path) || stream_due(&io, path))
        .cloned()
        .collect();
    for path in due {
        let Some(command) = io.queued_latest.remove(&path) else {
            continue;
        };
        let entered = io.queued_since.remove(&path);
        // A non-gesture drain is a committed value the view must adopt (same
        // contract as the ack-time drain in `finish_write`); a gesturing path
        // already shows the live thumb.
        if !io.active_gestures.contains_key(&path) {
            apply_optimistic(&path, &command.value, &bindings, &mut commands);
        }
        let write = IssuedWrite {
            request: WriteRequest {
                path: path.clone(),
                value: command.value.clone(),
                op_id: io.op_id(),
                if_revision: state.revision(&path),
            },
            meta: command.meta.clone(),
        };
        // Stamp regardless of outcome. A local failure is separately backed
        // off and retains the original queue timestamp; every attempt still
        // receives a fresh wire op id above.
        io.last_stream_issue.insert(path.clone(), Instant::now());
        if let Err(error) = issue_write(link.0.as_mut(), &mut io, write, 0) {
            handle_local_issue_failure(&mut io, &path, command, entered, error);
        } else if let Some(entered) = entered {
            io.lat_queue.record(entered.elapsed());
        }
    }
}

/// The pre-gesture authority a cancel must restore, with its adoption FLOOR:
/// the newest of any queued outbox intent, a still-in-flight write's value,
/// a Busy-retrying release, then server state — so a gesture starting while
/// the PREVIOUS release is still round-tripping baselines on that release.
/// The floor is the seed's PROVENANCE revision (a pending/retrying request's
/// own `if_revision`, not current state) — otherwise an external write that
/// already rebased state makes its own CAS rejection look non-newer and a
/// cancel would write our failed release back over external authority.
fn newest_desired_write<'a>(
    io: &'a MixerIo,
    path: &str,
    queued_revision: Option<u64>,
) -> Option<(&'a LeafValue, Option<u64>)> {
    if let Some(queued) = io.queued_latest.get(path) {
        return Some((&queued.value, queued_revision));
    }
    if let Some(pending) = io.pending.values().find_map(|kind| match kind {
        RequestKind::Write { write, .. } if write.request.path == path => {
            Some((&write.request.value, write.request.if_revision))
        }
        _ => None,
    }) {
        return Some(pending);
    }
    io.retries
        .iter()
        .rev()
        .find(|retry| retry.write.request.path == path)
        .map(|retry| (&retry.write.request.value, retry.write.request.if_revision))
}

fn desired_transport(state: &MusicdMixerState, io: Option<&MixerIo>) -> Option<DesiredTransport> {
    if state.connection != MixerConnectionState::Connected || !state.ready {
        return None;
    }
    if let Some((value, _)) = io.and_then(|io| {
        newest_desired_write(
            io,
            TRANSPORT_STATE_PATH,
            state.revision(TRANSPORT_STATE_PATH),
        )
    }) {
        return desired_transport_from_value(value, true);
    }
    state
        .value(TRANSPORT_STATE_PATH)
        .and_then(|value| desired_transport_from_value(value, false))
}

fn desired_transport_from_value(value: &LeafValue, provisional: bool) -> Option<DesiredTransport> {
    match value {
        LeafValue::Enum(value) if value == "playing" => Some(DesiredTransport {
            playing: true,
            provisional,
        }),
        LeafValue::Enum(value) if value == "stopped" => Some(DesiredTransport {
            playing: false,
            provisional,
        }),
        _ => None,
    }
}

fn seed_gesture_baseline(
    io: &MixerIo,
    state: &MusicdMixerState,
    path: &str,
    live_position: Option<f64>,
) -> Option<(LeafValue, Option<u64>)> {
    if let Some((value, revision)) = newest_desired_write(io, path, state.revision(path)) {
        return Some((value.clone(), revision));
    }
    // The stored transport.position leaf is the last SEEK TARGET, not where
    // the song actually is — cancelling a scrub restored from it would SEEK
    // the DSP backwards. Baseline a scrub on the live extrapolated clock
    // (revision floor still from state — the CAS provenance is unchanged).
    if path == TRANSPORT_POSITION_PATH {
        if let Some(live) = live_position {
            return Some((LeafValue::Number(live), state.revision(path)));
        }
    }
    state
        .value(path)
        .cloned()
        .map(|value| (value, state.revision(path)))
}

/// A Busy write whose path has newer queued intent (stream latest, a release,
/// or a cancel baseline) must NOT be retried — the retry would resurrect stale
/// intent over the newer write. Treating it as terminal lets `finish_write` /
/// the outbox drain the queued value instead.
fn busy_retry_superseded(io: &MixerIo, path: &str) -> bool {
    io.queued_latest.contains_key(path)
}

/// An inbound authoritative change during an active gesture updates the cancel
/// baseline ONLY when it is not an echo of one of our own writes — cancel then
/// yields to the concurrent external writer instead of stomping it. Attribution
/// prefers the published `source_id` (definitive, and immune to the
/// changed-before-ack race on the independent topic channel); when absent
/// (older daemon, snapshot leaves) it falls back to the revisions our acks
/// recorded — a snapshot carrying our not-yet-acked revision is the accepted
/// residual: it conservatively updates the baseline toward server state.
fn external_change_updates_baseline(
    io: &MixerIo,
    path: &str,
    revision: u64,
    source_id: Option<&str>,
    own_service: &str,
) -> bool {
    if !io.active_gestures.contains_key(path) {
        return false;
    }
    match source_id {
        Some(source) => source != own_service,
        None => !io
            .own_write_revisions
            .get(path)
            .is_some_and(|revisions| revisions.contains(&revision)),
    }
}

/// Fold an inbound authoritative (path, value, revision) into the cancel
/// baseline when it is (a) external per [`external_change_updates_baseline`]
/// and (b) NEWER than the baseline's adoption floor — a delayed pre-gesture
/// event must not rewind the baseline past authority we already captured.
fn maybe_update_gesture_baseline(
    io: &mut MixerIo,
    path: &str,
    revision: u64,
    value: &LeafValue,
    source_id: Option<&str>,
    own_service: &str,
) {
    if !external_change_updates_baseline(io, path, revision, source_id, own_service) {
        return;
    }
    let floor = io.gesture_baseline.get(path).and_then(|(_, floor)| *floor);
    if floor.is_none_or(|floor| revision > floor) {
        io.gesture_baseline
            .insert(path.to_string(), (value.clone(), Some(revision)));
    }
}

/// Clamp an app-issued seek into the transport domain; a non-positive length
/// is unbounded (matching [`extrapolate_position_seconds`]).
fn clamp_seek_seconds(seconds: f64, length_seconds: f64) -> f64 {
    if length_seconds > 0.0 {
        seconds.clamp(0.0, length_seconds)
    } else {
        seconds.max(0.0)
    }
}

/// The one reduction point for app seeks and bound controls. It preserves the
/// existing per-path serialization and CAS issue path; the envelope only adds
/// logical identity around that machinery.
fn submit_write(
    transport: Option<&mut MixerTransportRes>,
    state: &mut MusicdMixerState,
    io: &mut MixerIo,
    path: String,
    command: QueuedWrite,
    mode: SubmitMode,
) -> (CommandGeneration, bool) {
    let generation = command.meta.generation;

    // transport.position is exclusive while a gesture owns it: app seeks have
    // no entity and competing controls carry a different one, while the
    // owner's own stream continues through the same reducer.
    if path == TRANSPORT_POSITION_PATH
        && io
            .active_gestures
            .get(&path)
            .is_some_and(|active| command.meta.source.entity != Some(active.owner))
    {
        let reason = "transport position is owned by an active gesture".to_string();
        state.last_error = Some(reason.clone());
        io.complete(&path, command.meta, CommandTerminal::Rejected { reason });
        return (generation, false);
    }

    let connected = state.connection == MixerConnectionState::Connected && state.ready;
    if !connected || transport.is_none() {
        let reason = "mixer is not ready for writes".to_string();
        state.last_error = Some(reason.clone());
        io.complete(&path, command.meta, CommandTerminal::Rejected { reason });
        return (generation, false);
    }

    io.queue_write(&path, command);
    if mode == SubmitMode::Stream || io.inflight_paths.contains(&path) {
        return (generation, true);
    }

    let command = io
        .queued_latest
        .remove(&path)
        .expect("command was queued immediately above");
    let entered = io.queued_since.remove(&path);
    let write = IssuedWrite {
        request: WriteRequest {
            path: path.clone(),
            value: command.value.clone(),
            op_id: io.op_id(),
            // An app-initiated seek (RTZ / jump via `on_seek_request`, always
            // Discrete) is an ABSOLUTE, authoritative command — it must always
            // land, never be CAS-gated. The follower's per-path
            // `transport.position` revision drifts from the store's (it tracks
            // the global changed-event revision, while the leaf is only written
            // on seeks), so a conditional seek is spuriously rejected mid-play.
            // Gesture seeks (Stream/Commit) keep their CAS ordering.
            if_revision: if path == TRANSPORT_POSITION_PATH && mode == SubmitMode::Discrete {
                None
            } else {
                state.revision(&path)
            },
        },
        meta: command.meta.clone(),
    };
    if let Err(error) = issue_write(
        transport
            .expect("readiness check established a transport")
            .0
            .as_mut(),
        io,
        write,
        0,
    ) {
        state.last_error = Some(error.clone());
        // The local issue never reached the wire. Preserve the SAME logical
        // generation and queue timestamp; a later issue gets a fresh op_id.
        handle_local_issue_failure(io, &path, command, entered, error);
    }
    (generation, true)
}

fn issue_write(
    transport: &mut dyn MixerTransport,
    io: &mut MixerIo,
    write: IssuedWrite,
    attempt: u8,
) -> Result<(), String> {
    let request_id = io.request_id();
    // Stamp BEFORE the transport call: a synchronous transport completes (and
    // stamps `completed_at`) inside `issue_write`, so a post-call stamp would
    // make every fused ack measure zero. For the Bus arm this moves the stamp
    // across a bounded-channel try_send — sub-microsecond, and honestly part
    // of issuing.
    let issued = Instant::now();
    transport.issue_write(request_id, &write.request)?;
    io.inflight_paths.insert(write.request.path.clone());
    io.issued_at.insert(request_id, issued);
    if write.request.path == TRANSPORT_POSITION_PATH {
        io.seek_epoch = io.seek_epoch.wrapping_add(1);
    }
    io.pending
        .insert(request_id, RequestKind::Write { write, attempt });
    Ok(())
}

fn request_snapshot(transport: &mut dyn MixerTransport, io: &mut MixerIo) -> Result<(), String> {
    if io
        .pending
        .values()
        .any(|kind| matches!(kind, RequestKind::Snapshot))
    {
        return Ok(());
    }
    let request_id = io.request_id();
    transport.request_snapshot(request_id)?;
    io.snapshot_refresh_required = false;
    io.last_snapshot_request = Some(Instant::now());
    io.pending.insert(request_id, RequestKind::Snapshot);
    Ok(())
}

fn invalidate_inflight_snapshot(io: &mut MixerIo) {
    io.snapshot_refresh_required = true;
}

fn snapshot_reply_needs_replacement(io: &MixerIo) -> bool {
    io.snapshot_refresh_required
}

fn refresh_snapshot_if_due(
    transport: &mut dyn MixerTransport,
    state: &mut MusicdMixerState,
    io: &mut MixerIo,
) {
    if state.connection != MixerConnectionState::Connected
        || io
            .pending
            .values()
            .any(|kind| matches!(kind, RequestKind::Snapshot))
    {
        return;
    }
    let due = io
        .last_snapshot_request
        .is_none_or(|last| last.elapsed() >= SNAPSHOT_REFRESH_INTERVAL);
    if due {
        if let Err(error) = request_snapshot(transport, io) {
            state.last_error = Some(error);
        }
    }
}

#[allow(clippy::too_many_arguments)] // The pump owns every resource a reply can touch.
fn pump_transport(
    mut link: ResMut<MixerTransportRes>,
    mut poll: Local<TransportPoll>,
    mut state: ResMut<MusicdMixerState>,
    mut io: ResMut<MixerIo>,
    mut latest_meter: ResMut<LatestMeter>,
    mut transport: ResMut<TransportPosition>,
    bindings: Query<(Entity, &MixerBinding)>,
    mut names: Query<(&MixerName, &mut Text)>,
    mut commands: Commands,
) {
    let link = link.0.as_mut();
    // Events FIRST, and messages are not even drained (let alone decoded)
    // until every reply in this batch is reconciled — the pre-seam order, so
    // telemetry decode cost can never inflate a measured issue→ack.
    link.poll_events(&mut poll.events);
    // Overflow invalidates a snapshot that was already in flight: it may have
    // been captured before the dropped incremental change. Mark it before
    // processing replies so event ordering within this drained batch cannot
    // briefly make that snapshot authoritative.
    if poll
        .events
        .iter()
        .any(|event| matches!(event, TransportEvent::DroppedMessages(_)))
    {
        invalidate_inflight_snapshot(&mut io);
    }
    let mut snapshot_this_frame = None;
    let mut epoch_reset_this_frame = false;
    let epoch_fence_at_frame_start = io.epoch_fence;

    for event in poll.events.drain(..) {
        match event {
            TransportEvent::Connection {
                state: connection,
                generation,
            } => {
                state.connection = connection;
                if connection == MixerConnectionState::Connected {
                    // Preserve the last revision long enough for the new
                    // snapshot to detect a restarted authority. Writes remain
                    // gated by `ready` until that snapshot lands.
                    state.ready = false;
                    state.last_applied_revision = None;
                    io.abandon_commands(LifecycleReset::ConnectionGenerationChanged);
                    io.sync_generation = generation;
                    io.buffered_changes.clear();
                    io.last_snapshot_request = None;
                    io.inflight_paths.clear();
                    io.active_gestures.clear();
                    // Stream bookkeeping is generation-scoped too: a baseline
                    // captured under the old epoch must never be written back
                    // after a resync re-established authority.
                    io.gesture_baseline.clear();
                    io.own_write_revisions.clear();
                    io.last_stream_issue.clear();
                    io.queued_since.clear();
                    io.issued_at.clear();
                    // The extrapolation base is generation-scoped telemetry:
                    // the changed topic / next poll re-establish it.
                    *transport = TransportPosition::default();
                    if let Err(error) = request_snapshot(link, &mut io) {
                        state.last_error = Some(error);
                    }
                }
            }
            TransportEvent::Reply {
                request_id,
                result,
                completed_at,
            } => {
                let Some(kind) = io.pending.remove(&request_id) else {
                    continue;
                };
                if matches!(&kind, RequestKind::Snapshot) && snapshot_reply_needs_replacement(&io) {
                    // Do not apply a snapshot that may predate a known dropped
                    // change. The pending slot has now been removed, so replace
                    // it immediately rather than waiting for the periodic poll.
                    io.last_snapshot_request = None;
                    if let Err(error) = request_snapshot(link, &mut io) {
                        state.last_error = Some(error);
                    }
                    continue;
                }
                match (kind, result) {
                    (RequestKind::Snapshot, Ok(TransportReply::Snapshot(snapshot))) => {
                        if let Some((revision, epoch_reset)) = apply_snapshot(
                            snapshot,
                            &mut state,
                            &mut io,
                            &bindings,
                            &mut names,
                            &mut commands,
                        ) {
                            snapshot_this_frame = Some(revision);
                            epoch_reset_this_frame |= epoch_reset;
                        }
                    }
                    // Rejection, decode failure and transport failure all
                    // carry their (pre-seam-identical) message in the error.
                    (RequestKind::Snapshot, Err(error)) => {
                        handle_snapshot_failure(&mut state, &mut io, error);
                    }
                    // A reply variant that does not match its request kind is
                    // a transport bug; surface it like a snapshot failure.
                    (RequestKind::Snapshot, Ok(_)) => {
                        handle_snapshot_failure(
                            &mut state,
                            &mut io,
                            "snapshot reply type mismatch".to_string(),
                        );
                    }
                    (RequestKind::Write { write, attempt }, Ok(TransportReply::Write(outcome))) => {
                        let path = write.request.path.clone();
                        let issued = io.issued_at.remove(&request_id);
                        if let Some(issued) = issued {
                            // An in-process transport stamps the instant its
                            // synchronous call returned (ack = the call's
                            // return); the Bus arm leaves None and is stamped
                            // here at drain — the pre-seam semantics.
                            let sample = completed_at
                                .map(|done| done.saturating_duration_since(issued))
                                .unwrap_or_else(|| issued.elapsed());
                            io.lat_rtt.record(sample);
                        }
                        let terminal = handle_write_reply(
                            write,
                            attempt,
                            outcome,
                            issued,
                            &mut state,
                            &mut io,
                            &mut transport,
                            &bindings,
                            &mut names,
                            &mut commands,
                        );
                        if terminal {
                            finish_write(&path, &mut io);
                        }
                    }
                    (RequestKind::Write { write, .. }, Err(error)) => {
                        io.issued_at.remove(&request_id);
                        state.last_error = Some(error.clone());
                        restore_path_unless_gesturing(
                            &write.request.path,
                            &state,
                            &io,
                            &bindings,
                            &mut commands,
                        );
                        io.inflight_paths.remove(&write.request.path);
                        io.complete(
                            &write.request.path,
                            write.meta,
                            CommandTerminal::Rejected { reason: error },
                        );
                        // Deliberately KEEP any queued value: it is durable
                        // intent (possibly a cancel baseline) and the outbox
                        // retries it; a real disconnect clears it via resync.
                    }
                    (RequestKind::Write { write, .. }, Ok(_)) => {
                        // Kind/variant mismatch (transport bug) — treat like a
                        // transport failure so the path is released.
                        io.issued_at.remove(&request_id);
                        let error = "write reply type mismatch".to_string();
                        state.last_error = Some(error.clone());
                        restore_path_unless_gesturing(
                            &write.request.path,
                            &state,
                            &io,
                            &bindings,
                            &mut commands,
                        );
                        io.inflight_paths.remove(&write.request.path);
                        io.complete(
                            &write.request.path,
                            write.meta,
                            CommandTerminal::Rejected { reason: error },
                        );
                    }
                    (
                        RequestKind::PositionPoll {
                            generation,
                            seek_epoch,
                            position_revision,
                        },
                        Ok(TransportReply::Position(seconds)),
                    ) => {
                        // The live transport clock in SECONDS (the daemon
                        // divides live frames by SR). Discard when provenance
                        // changed: an old connection generation predates a
                        // reconnect's authority, an old seek epoch means the
                        // poll raced a seek we issued after it, and live seek
                        // intent (queued or in flight) must never be
                        // overwritten by a pre-seek reading.
                        let seek_intent_active =
                            io.queued_latest.contains_key(TRANSPORT_POSITION_PATH)
                                || io.inflight_paths.contains(TRANSPORT_POSITION_PATH);
                        if generation == io.sync_generation
                            && seek_epoch == io.seek_epoch
                            && position_revision == state.revision(TRANSPORT_POSITION_PATH)
                            && !seek_intent_active
                        {
                            transport.base_seconds = seconds;
                            transport.base_at = Some(Instant::now());
                            transport.playing = transport_is_playing(&state);
                        }
                    }
                    // A rejected/undecodable/failed poll is transient (the
                    // clock is read-only telemetry) — the next poll retries.
                    (RequestKind::PositionPoll { .. }, _) => {}
                }
            }
            TransportEvent::DroppedMessages(count) => {
                state.last_error = Some(format!("Bus bridge dropped {count} inbound messages"));
                if state.connection == MixerConnectionState::Connected {
                    if let Err(error) = request_snapshot(link, &mut io) {
                        state.last_error = Some(error);
                    }
                }
            }
            TransportEvent::Fatal(error) => {
                state.connection = MixerConnectionState::Fatal;
                state.ready = false;
                state.last_error = Some(error);
            }
        }
    }

    if let Some(revision) = snapshot_this_frame {
        for changed in take_replayable_changes(&mut io, revision) {
            let update_view = should_update_view(&io, &changed.path);
            apply_leaf(
                changed.path,
                changed.value,
                changed.revision,
                update_view,
                &mut state,
                &bindings,
                &mut names,
                &mut commands,
            );
        }
    }

    // Replies are reconciled; only now does the transport drain + decode
    // this frame's telemetry.
    link.poll_messages(&mut poll.messages);

    if epoch_reset_this_frame || epoch_fence_at_frame_start {
        // Same-generation topic messages may have been queued by the previous
        // musicd process before the restart snapshot. Their high revisions are
        // indistinguishable from fresh changes, so drop this drained batch and
        // immediately confirm the new authority with another snapshot.
        poll.messages.clear();
        link.discard_backlog();
        if epoch_reset_this_frame {
            io.last_snapshot_request = None;
            // A restarted authority's clock has no relation to the old one,
            // and the replacement snapshot deliberately does not rebase it
            // (its position leaf is the stored seek target) — reset and let
            // the changed topic / next poll re-establish the base.
            *transport = TransportPosition::default();
        }
    }

    for message in poll.messages.drain(..) {
        if message.generation() != io.sync_generation {
            continue;
        }
        // Meters keep flowing through the epoch fence; changed/applied (and a
        // malformed frame of either) wait for the confirming snapshot.
        if io.epoch_fence && !matches!(message, TransportMessage::Meter { .. }) {
            continue;
        }
        match message {
            TransportMessage::Meter { frame, .. } => latest_meter.0 = Some(frame),
            TransportMessage::Malformed { error, .. } => state.last_error = Some(error),
            TransportMessage::Changed { event: changed, .. } => {
                {
                    let snapshot_pending = io
                        .pending
                        .values()
                        .any(|kind| matches!(kind, RequestKind::Snapshot));
                    if !state.ready {
                        buffer_change(&mut io, changed);
                        continue;
                    }
                    if snapshot_pending {
                        // A periodic snapshot can race an incremental update.
                        // Apply it now for responsiveness and retain a copy so a
                        // successful older snapshot can replay it afterwards.
                        buffer_change(&mut io, changed.clone());
                    }
                    if snapshot_covers_change(snapshot_this_frame, changed.revision) {
                        continue;
                    }
                    maybe_update_gesture_baseline(
                        &mut io,
                        &changed.path,
                        changed.revision,
                        &changed.value,
                        changed.source_id.as_deref(),
                        link.service_name(),
                    );
                    // Authoritative live-position events rebase the
                    // extrapolation clock (a failed poll then degrades to the
                    // ~10 Hz changed feed instead of drifting forever) — but
                    // only at-or-above the per-path revision state knows, so a
                    // delayed OLDER event that apply_leaf will reject cannot
                    // rewind the clock (equal revisions retained: transient
                    // position updates reuse a revision). Snapshots
                    // deliberately do NOT rebase it: their transport.position
                    // leaf is the stored SEEK TARGET, not the live clock.
                    if changed.path == TRANSPORT_POSITION_PATH
                        && state
                            .revision(TRANSPORT_POSITION_PATH)
                            .is_none_or(|current| changed.revision >= current)
                    {
                        if let LeafValue::Number(seconds) = &changed.value {
                            transport.base_seconds = *seconds;
                            transport.base_at = Some(Instant::now());
                            transport.playing = transport_is_playing(&state);
                        }
                    }
                    let update_view = should_update_view(&io, &changed.path);
                    apply_leaf(
                        changed.path,
                        changed.value,
                        changed.revision,
                        update_view,
                        &mut state,
                        &bindings,
                        &mut names,
                        &mut commands,
                    );
                }
            }
            TransportMessage::Applied { applied, .. } => {
                let awaiting = std::mem::take(&mut io.awaiting_applied);
                for awaiting in awaiting {
                    if awaiting.accepted_revision <= applied.revision {
                        if let Some(issued) = awaiting.issued {
                            io.lat_applied.record(issued.elapsed());
                        }
                        io.complete(
                            &awaiting.path,
                            awaiting.meta,
                            CommandTerminal::CoveredByAppliedRevision {
                                accepted_revision: awaiting.accepted_revision,
                                applied_revision: applied.revision,
                            },
                        );
                    } else {
                        io.awaiting_applied.push(awaiting);
                    }
                }
                state.last_applied_revision = Some(applied.revision);
            }
        }
    }

    refresh_snapshot_if_due(link, &mut state, &mut io);
}

fn snapshot_covers_change(snapshot_revision: Option<u64>, change_revision: u64) -> bool {
    snapshot_revision.is_some_and(|revision| change_revision < revision)
}

fn should_update_view(io: &MixerIo, path: &str) -> bool {
    !io.inflight_paths.contains(path) && !io.active_gestures.contains_key(path)
}

fn write_reply_updates_view(io: &MixerIo, path: &str) -> bool {
    !io.active_gestures.contains_key(path)
}

fn apply_snapshot(
    snapshot: MixerSnapshotResponse,
    state: &mut MusicdMixerState,
    io: &mut MixerIo,
    bindings: &Query<(Entity, &MixerBinding)>,
    names: &mut Query<(&MixerName, &mut Text)>,
    commands: &mut Commands,
) -> Option<(u64, bool)> {
    let revision = snapshot.revision;
    let epoch_reset = match state.begin_snapshot(&snapshot) {
        SnapshotDisposition::ConfirmEpoch => {
            // Do not mix a possibly restarted authority into the existing
            // cache. Ask again immediately; a second lagging snapshot confirms
            // the new epoch, while a raced refresh catches up normally.
            io.buffered_changes.clear();
            io.last_snapshot_request = None;
            return None;
        }
        SnapshotDisposition::EpochReset => {
            reset_epoch_io(io);
            io.epoch_fence = true;
            true
        }
        SnapshotDisposition::Applied => false,
    };
    for leaf in snapshot.leaves {
        // Snapshot leaves carry no writer identity; unattributed revisions on
        // a gesturing path conservatively update the cancel baseline toward
        // server authority (the CAS on the write-back guards the rest).
        maybe_update_gesture_baseline(io, &leaf.path, leaf.revision, &leaf.value, None, "");
        let update_view = should_update_view(io, &leaf.path);
        apply_leaf(
            leaf.path,
            leaf.value,
            leaf.revision,
            update_view,
            state,
            bindings,
            names,
            commands,
        );
    }
    if !epoch_reset {
        io.epoch_fence = false;
    }
    Some((revision, epoch_reset))
}

fn reset_epoch_io(io: &mut MixerIo) {
    io.abandon_commands(LifecycleReset::AuthorityEpochChanged);
    io.inflight_paths.clear();
    io.active_gestures.clear();
    io.gesture_baseline.clear();
    io.own_write_revisions.clear();
    io.last_stream_issue.clear();
    io.queued_since.clear();
    io.issued_at.clear();
    io.buffered_changes.clear();
    io.snapshot_refresh_required = false;
}

fn handle_snapshot_failure(state: &mut MusicdMixerState, io: &mut MixerIo, error: String) {
    state.last_error = Some(error);
    if state.ready {
        // Periodic-refresh changes were already applied live; their buffered
        // copies are only needed if the snapshot succeeds and overwrites them.
        io.buffered_changes.clear();
    }
}

fn buffer_change(io: &mut MixerIo, changed: ChangedEvent) {
    let replace = io
        .buffered_changes
        .get(&changed.path)
        .is_none_or(|current| changed.revision >= current.event.revision);
    if replace {
        io.buffered_changes.insert(
            changed.path.clone(),
            BufferedChange {
                sync_generation: io.sync_generation,
                event: changed,
            },
        );
    }
}

fn take_replayable_changes(io: &mut MixerIo, snapshot_revision: u64) -> Vec<ChangedEvent> {
    let sync_generation = io.sync_generation;
    let mut buffered: Vec<_> = std::mem::take(&mut io.buffered_changes)
        .into_values()
        .filter(|change| {
            // Equal revisions are normally idempotent, but transient values
            // such as transport.position can advance repeatedly at the same
            // global control revision. musicd's revisioned snapshot carries
            // the stored seek target, not the live RT clock, so the latest
            // buffered progress value remains the newer view authority.
            change.sync_generation == sync_generation && change.event.revision >= snapshot_revision
        })
        .map(|change| change.event)
        .collect();
    buffered.sort_unstable_by_key(|change| change.revision);
    buffered
}

#[allow(clippy::too_many_arguments)] // Reconciliation needs the ECS views it updates atomically.
fn handle_write_reply(
    write: IssuedWrite,
    attempt: u8,
    outcome: Result<WriteResponse, String>,
    issued: Option<Instant>,
    state: &mut MusicdMixerState,
    io: &mut MixerIo,
    transport: &mut TransportPosition,
    bindings: &Query<(Entity, &MixerBinding)>,
    names: &mut Query<(&MixerName, &mut Text)>,
    commands: &mut Commands,
) -> bool {
    let IssuedWrite { request, meta } = write;
    match outcome {
        Ok(WriteResponse::Accepted(ack)) => {
            // An accepted seek IS the authoritative clock — but only rebase
            // AFTER apply_leaf accepts the revision: a delayed ack must not
            // overwrite a newer external seek that telemetry already
            // delivered. Captured here; applied below on acceptance.
            let seek_seconds = if ack.path == TRANSPORT_POSITION_PATH {
                match &ack.canonical_value {
                    LeafValue::Number(seconds) => Some(*seconds),
                    _ => None,
                }
            } else {
                None
            };
            // Remember which revisions WE authored while a gesture is active —
            // the discriminator that stops our own changed-event echoes from
            // being treated as external writers for the cancel baseline.
            if io.active_gestures.contains_key(&ack.path) {
                io.own_write_revisions
                    .entry(ack.path.clone())
                    .or_default()
                    .insert(ack.revision);
            }
            // `dsp.applied` is a monotonic coverage watermark, not proof that
            // this exact value survived musicd's control-snapshot coalescing.
            // Track every accepted command even when its issue timestamp is
            // unavailable; the timestamp is optional instrumentation only.
            if let Some(applied_revision) = state
                .last_applied_revision
                .filter(|applied| *applied >= ack.revision)
            {
                if let Some(issued) = issued {
                    io.lat_applied.record(issued.elapsed());
                }
                io.complete(
                    &ack.path,
                    meta,
                    CommandTerminal::CoveredByAppliedRevision {
                        accepted_revision: ack.revision,
                        applied_revision,
                    },
                );
            } else {
                if io.awaiting_applied.len() >= 256 {
                    let evicted = io.awaiting_applied.remove(0);
                    io.complete(
                        &evicted.path,
                        evicted.meta,
                        CommandTerminal::CoverageUnknown {
                            accepted_revision: evicted.accepted_revision,
                            reason: "applied-coverage tracker capacity exceeded".to_string(),
                        },
                    );
                }
                io.awaiting_applied.push(AwaitingCoverage {
                    path: ack.path.clone(),
                    accepted_revision: ack.revision,
                    issued,
                    meta,
                });
            }
            let update_view = write_reply_updates_view(io, &ack.path);
            let accepted = apply_leaf(
                ack.path,
                ack.canonical_value,
                ack.revision,
                update_view,
                state,
                bindings,
                names,
                commands,
            );
            if accepted {
                // Revision-current seek ack: rebase the clock so the
                // self-healing follow converges to the seek target instead of
                // snapping the thumb back to the stale pre-seek position
                // until the changed echo / next poll (RTZ must stick).
                if let Some(seconds) = seek_seconds {
                    transport.base_seconds = seconds;
                    transport.base_at = Some(Instant::now());
                    transport.playing = transport_is_playing(state);
                }
            } else if update_view {
                restore_path(&request.path, state, bindings, commands);
            }
            state.last_error = None;
            true
        }
        Ok(WriteResponse::Rejected(rejection)) => {
            let reason = rejection.reason.clone();
            // A CAS rejection is positive proof of a writer we haven't seen —
            // and the coalesced changed topic may never republish their value.
            // Fold it into the cancel baseline (revision fallback guards the
            // one benign case: our own earlier ack racing this reply).
            maybe_update_gesture_baseline(
                io,
                &rejection.path,
                rejection.current_revision,
                &rejection.current_value,
                None,
                "",
            );
            let update_view = write_reply_updates_view(io, &rejection.path);
            let accepted = apply_leaf(
                rejection.path.clone(),
                rejection.current_value.clone(),
                rejection.current_revision,
                update_view,
                state,
                bindings,
                names,
                commands,
            );
            if !accepted {
                // The monotonic guard refused the store's authoritative
                // current_revision because it is LOWER than our stale tracked
                // value (an authority epoch / song-bank swap rolled the leaf
                // back). Recovering from that is legitimate ONLY for
                // `transport.position`: every other leaf recovers via the
                // snapshot epoch-reset (`begin_snapshot`), from which position
                // is deliberately exempt — so without this its CAS token stays
                // stuck ahead and every future seek re-rejects (the ruler-drag
                // + load-reset wedge). Scoped tightly per codex review
                // (019f7d4a): a DELAYED rejection on an ordinary leaf can arrive
                // after a newer CHANGED already advanced it, and force-syncing
                // there would rewind authoritative state; and we skip a position
                // rejection while a scrub gesture owns the leaf, so we never
                // rebase out from under the gesture's cancel baseline /
                // own-write provenance.
                let position_wedge = rejection.path == TRANSPORT_POSITION_PATH
                    && !io.active_gestures.contains_key(TRANSPORT_POSITION_PATH);
                if position_wedge {
                    // Revision AND value together — a revision-only rebase would
                    // leave an incoherent cached pair.
                    state.resync_leaf(
                        rejection.path.clone(),
                        rejection.current_value,
                        rejection.current_revision,
                    );
                }
                if update_view {
                    restore_path(&request.path, state, bindings, commands);
                }
            }
            state.last_error = Some(reason.clone());
            io.complete(&request.path, meta, CommandTerminal::Rejected { reason });
            true
        }
        Ok(WriteResponse::Busy(_)) if attempt < 5 => {
            if busy_retry_superseded(io, &request.path) {
                let (same_lineage, by) = {
                    let queued = io
                        .queued_latest
                        .get(&request.path)
                        .expect("supersession check observed a queued command");
                    (
                        same_gesture_lineage(&meta, &queued.meta),
                        queued.meta.generation,
                    )
                };
                if !same_lineage {
                    io.complete(&request.path, meta, CommandTerminal::Superseded { by });
                }
                return true;
            }
            let delay_ms = 5u64 << attempt;
            io.retries.push(RetryWrite {
                due: Instant::now() + Duration::from_millis(delay_ms),
                write: IssuedWrite { request, meta },
                attempt: attempt + 1,
            });
            false
        }
        Ok(WriteResponse::Busy(busy)) => {
            let reason = format!("{} after retries", busy.reason);
            state.last_error = Some(reason.clone());
            restore_path_unless_gesturing(&request.path, state, io, bindings, commands);
            io.complete(&request.path, meta, CommandTerminal::Rejected { reason });
            true
        }
        Err(error) => {
            let reason = format!("write response decode: {error}");
            state.last_error = Some(reason.clone());
            restore_path_unless_gesturing(&request.path, state, io, bindings, commands);
            io.complete(&request.path, meta, CommandTerminal::Rejected { reason });
            true
        }
    }
}

fn finish_write(path: &str, io: &mut MixerIo) {
    io.inflight_paths.remove(path);
    // Any queued value (streaming latest, a release, a cancel baseline) is
    // drained exclusively by the durable outbox (`flush_queued_writes`, same
    // frame) — one issuer, one failure-handling path, no lost intent.
}

fn apply_optimistic(
    path: &str,
    value: &LeafValue,
    bindings: &Query<(Entity, &MixerBinding)>,
    commands: &mut Commands,
) {
    for (entity, binding) in bindings.iter() {
        if binding.path != path {
            continue;
        }
        match value {
            LeafValue::Number(number) => commands.trigger(SetControlValue {
                source: entity,
                value: *number as f32,
            }),
            LeafValue::Bool(boolean) => commands.trigger(SetToggleValue {
                source: entity,
                value: *boolean,
            }),
            LeafValue::Enum(_) => {}
        }
    }
}

fn retry_busy_writes(
    mut link: ResMut<MixerTransportRes>,
    mut state: ResMut<MusicdMixerState>,
    mut io: ResMut<MixerIo>,
    bindings: Query<(Entity, &MixerBinding)>,
    mut commands: Commands,
) {
    let now = Instant::now();
    let mut due = Vec::new();
    let mut waiting = Vec::new();
    for retry in io.retries.drain(..) {
        if retry.due <= now {
            due.push(retry)
        } else {
            waiting.push(retry)
        }
    }
    io.retries = waiting;
    for retry in due {
        let path = retry.write.request.path.clone();
        // Newer queued intent supersedes the retried value — release the
        // in-flight claim and let the outbox issue the newer write instead.
        if busy_retry_superseded(&io, &path) {
            let (same_lineage, by) = {
                let queued = io
                    .queued_latest
                    .get(&path)
                    .expect("supersession check observed a queued command");
                (
                    same_gesture_lineage(&retry.write.meta, &queued.meta),
                    queued.meta.generation,
                )
            };
            if !same_lineage {
                io.complete(&path, retry.write.meta, CommandTerminal::Superseded { by });
            }
            io.inflight_paths.remove(&path);
            continue;
        }
        if state.connection != MixerConnectionState::Connected {
            let reason = "mixer disconnected during a busy-write retry".to_string();
            state.last_error = Some(reason.clone());
            io.inflight_paths.remove(&path);
            restore_path_unless_gesturing(&path, &state, &io, &bindings, &mut commands);
            io.complete(
                &path,
                retry.write.meta,
                CommandTerminal::Rejected { reason },
            );
            continue;
        }
        if !state.ready {
            io.retries.push(RetryWrite {
                due: Instant::now() + Duration::from_millis(20),
                write: retry.write,
                attempt: retry.attempt,
            });
            continue;
        }
        if let Err(error) =
            issue_write(link.0.as_mut(), &mut io, retry.write.clone(), retry.attempt)
        {
            state.last_error = Some(error);
            if retry.attempt < 5 {
                io.retries.push(RetryWrite {
                    due: Instant::now() + Duration::from_millis(20),
                    write: retry.write,
                    attempt: retry.attempt + 1,
                });
            } else {
                io.inflight_paths.remove(&path);
                io.complete(
                    &path,
                    retry.write.meta,
                    CommandTerminal::Rejected {
                        reason: "local issue failed after retries".to_string(),
                    },
                );
                // Queued intent (if any) stays: the durable outbox retries it.
                restore_path_unless_gesturing(&path, &state, &io, &bindings, &mut commands);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)] // One reconciliation point updates state and its ECS views.
fn apply_leaf(
    path: String,
    value: LeafValue,
    revision: u64,
    update_view: bool,
    state: &mut MusicdMixerState,
    bindings: &Query<(Entity, &MixerBinding)>,
    names: &mut Query<(&MixerName, &mut Text)>,
    commands: &mut Commands,
) -> bool {
    if !state.accept_leaf(path.clone(), value.clone(), revision) {
        return false;
    }
    // The scrubber's view is owned by the transport CLOCK
    // (`update_transport_position`), never by leaf state: the stored
    // `transport.position` leaf is the last SEEK TARGET (usually 0), so a
    // periodic snapshot refresh re-applying it here would flick the thumb
    // to zero every 2s — and park it there whenever the transport is
    // stopped mid-song.
    if !update_view || path == TRANSPORT_POSITION_PATH {
        return true;
    }
    for (entity, binding) in bindings.iter() {
        if binding.path == path {
            match &value {
                LeafValue::Number(number) => commands.trigger(SetControlValue {
                    source: entity,
                    value: *number as f32,
                }),
                LeafValue::Bool(boolean) => commands.trigger(SetToggleValue {
                    source: entity,
                    value: *boolean,
                }),
                LeafValue::Enum(_) => {}
            }
        }
    }
    if let LeafValue::Enum(text) = value {
        for (name, mut rendered) in names.iter_mut() {
            if name.path == path {
                let shown = if text.is_empty() {
                    &name.fallback
                } else {
                    &text
                };
                rendered.0 = render_mixer_name(name, shown);
            }
        }
    }
    true
}

fn restore_path(
    path: &str,
    state: &MusicdMixerState,
    bindings: &Query<(Entity, &MixerBinding)>,
    commands: &mut Commands,
) {
    // Same ownership rule as apply_leaf: restoring the scrubber to the
    // stored seek-target leaf would rewind it; the clock re-asserts the
    // live position within a frame instead.
    if path == TRANSPORT_POSITION_PATH || state.value(path).is_none() {
        return;
    }
    for (entity, binding) in bindings.iter() {
        if binding.path == path {
            restore_authoritative(path, entity, state, commands);
        }
    }
}

fn restore_path_unless_gesturing(
    path: &str,
    state: &MusicdMixerState,
    io: &MixerIo,
    bindings: &Query<(Entity, &MixerBinding)>,
    commands: &mut Commands,
) {
    if !io.active_gestures.contains_key(path) {
        restore_path(path, state, bindings, commands);
    }
}

fn restore_authoritative(
    path: &str,
    entity: Entity,
    state: &MusicdMixerState,
    commands: &mut Commands,
) {
    if let Some(value) = state.value(path) {
        set_control_from_value(entity, value, commands);
    }
}

/// Programmatically set a control's view to `value` (no `ControlChange` echo —
/// the Set* events are the non-emitting write path by construction).
fn set_control_from_value(entity: Entity, value: &LeafValue, commands: &mut Commands) {
    match value {
        LeafValue::Number(number) => commands.trigger(SetControlValue {
            source: entity,
            value: *number as f32,
        }),
        LeafValue::Bool(boolean) => commands.trigger(SetToggleValue {
            source: entity,
            value: *boolean,
        }),
        LeafValue::Enum(_) => {}
    }
}

/// Newly spawned bound widgets adopt the current authoritative value at once.
/// Bindings spawned after the initial snapshot (dynamic views — arranger lane
/// M/S, rebuilt structures) must not sit on widget defaults until the next
/// snapshot refresh; a click in that stale window would write the wrong
/// semantic action (e.g. mute=true on an already-muted channel).
fn seed_added_bindings(
    state: Res<MusicdMixerState>,
    io: Res<MixerIo>,
    added: Query<(Entity, &MixerBinding), bevy::ecs::query::Added<MixerBinding>>,
    mut commands: Commands,
) {
    for (entity, binding) in &added {
        if should_update_view(&io, &binding.path) {
            restore_authoritative(&binding.path, entity, &state, &mut commands);
        }
    }
}

fn apply_latest_meter(
    mut latest: ResMut<LatestMeter>,
    mut meters: Query<(&MixerMeterBinding, &mut MeterValue)>,
) {
    let Some(frame) = latest.0.take() else { return };
    for (binding, mut meter) in &mut meters {
        let Some(record) = frame.records.get(binding.0) else {
            continue;
        };
        let next = [
            MeterLane {
                level: db_to_meter_position(from_centi_dbfs(record.rms_l) as f32),
                peak: db_to_meter_position(from_centi_dbfs(record.peak_l) as f32),
                hold: db_to_meter_position(from_centi_dbfs(record.hold_l) as f32),
                clipped: record.clip & 1 != 0,
            },
            MeterLane {
                level: db_to_meter_position(from_centi_dbfs(record.rms_r) as f32),
                peak: db_to_meter_position(from_centi_dbfs(record.peak_r) as f32),
                hold: db_to_meter_position(from_centi_dbfs(record.hold_r) as f32),
                clipped: record.clip & 2 != 0,
            },
        ];
        // Idle channels repeat identical frames at 60Hz; comparing through the
        // immutable deref skips the write (and Bevy's change flag), so their
        // meter visuals never re-run. 31 of 33 meters are silent in a
        // single-stem session — this is most of the board's frame cost.
        if meter.lanes != next {
            meter.lanes = next;
        }
    }
}

pub fn db_to_meter_position(db: f32) -> f32 {
    if db <= -60.0 {
        0.0
    } else {
        ((db + 60.0) / 66.0).clamp(0.0, 1.0)
    }
}

pub fn default_fader_mapping() -> ValueMapping {
    ValueMapping::piecewise([
        (0.0, FADER_MIN_DB as f32),
        (0.10, -60.0),
        (0.25, -30.0),
        (0.50, -12.0),
        (0.75, 0.0),
        (1.0, FADER_MAX_DB as f32),
    ])
    .expect("static mixer fader mapping is valid")
}

/// Extrapolate the live transport position (seconds) from the last polled
/// `base_seconds`, advancing by `elapsed_seconds` of wall-clock only while
/// `playing`, then clamping to `[0, length_seconds]`. A non-positive
/// `length_seconds` is unbounded (the benchmark multitone source) and imposes
/// no upper clamp — matching the daemon, which leaves an unbounded length
/// unclamped on write. Pure so the timing math is unit-testable.
pub fn extrapolate_position_seconds(
    base_seconds: f64,
    elapsed_seconds: f64,
    playing: bool,
    length_seconds: f64,
) -> f64 {
    let advanced = if playing {
        base_seconds + elapsed_seconds.max(0.0)
    } else {
        base_seconds
    };
    let advanced = advanced.max(0.0);
    if length_seconds > 0.0 {
        advanced.min(length_seconds)
    } else {
        advanced
    }
}

/// Format a duration in seconds as `M:SS` (a negative or NaN input reads
/// `0:00`). Used for both halves of the transport `position / length` readout.
fn format_mmss(seconds: f64) -> String {
    let total = if seconds.is_finite() && seconds > 0.0 {
        seconds as u64
    } else {
        0
    };
    format!("{}:{:02}", total / 60, total % 60)
}

/// The scrubber's domain range + linear mapping for a song of `length_secs`
/// (unbounded → [`SCRUBBER_FALLBACK_LENGTH_SECS`]). Clamped to a sane finite
/// span so an absurd length can never produce an invalid mapping.
fn scrubber_range_for(length_secs: f32) -> (ControlRange, ValueMapping) {
    let max = if length_secs > 0.0 {
        length_secs
    } else {
        SCRUBBER_FALLBACK_LENGTH_SECS
    }
    .clamp(1.0, 1_000_000.0);
    (
        ControlRange {
            min: 0.0,
            max,
            step: 0.0,
            detent: None,
        },
        ValueMapping::linear(0.0, max).expect("scrubber range is valid"),
    )
}

fn leaf_number(value: Option<&LeafValue>) -> Option<f64> {
    match value {
        Some(LeafValue::Number(n)) => Some(*n),
        _ => None,
    }
}

/// True when the store's `transport.state` leaf is `playing`. Public so app
/// playheads (piano roll, arranger lanes) follow the same clock the scrubber
/// does.
pub fn transport_is_playing(state: &MusicdMixerState) -> bool {
    matches!(state.value(TRANSPORT_STATE_PATH), Some(LeafValue::Enum(s)) if s == "playing")
}

/// The store's `transport.length` in seconds (0 = unbounded / unknown).
pub fn transport_length_secs(state: &MusicdMixerState) -> f32 {
    leaf_number(state.value(TRANSPORT_LENGTH_PATH)).unwrap_or(0.0) as f32
}

fn update_readouts(
    controls: Query<&ControlValue, Changed<ControlValue>>,
    mut readouts: Query<(&MixerReadout, &mut Text)>,
) {
    for (readout, mut text) in &mut readouts {
        let Ok(value) = controls.get(readout.control) else {
            continue;
        };
        text.0 = format!("{:.*}{}", readout.precision, value.0, readout.suffix);
    }
}

fn update_status_text(state: Res<MusicdMixerState>, mut texts: Query<&mut StatusText>) {
    if texts.is_empty() {
        return;
    }
    let audio = if state.real_audio {
        "real audio"
    } else {
        "no-device fallback"
    };
    // "Link", not "Bus": the same status line serves the fused arm, whose
    // link is a function call. Identical text across arms by construction.
    let mut status = format!("Link: {:?} | {audio}", state.connection);
    if state.connection == MixerConnectionState::Connected && !state.ready {
        status.push_str(" | resyncing");
    }
    if let Some(revision) = state.snapshot_revision {
        status.push_str(&format!(" | revision {revision}"));
    }
    if state.audio_fault || state.applied_fault {
        status.push_str(" | ENGINE FAULT");
    }
    if let Some(error) = &state.last_error {
        status.push_str(&format!(" | {error}"));
    }
    for mut text in &mut texts {
        if text.0 != status {
            text.set(status.clone());
        }
    }
}

fn request_position(
    transport: &mut dyn MixerTransport,
    state: &MusicdMixerState,
    io: &mut MixerIo,
) -> Result<(), String> {
    let request_id = io.request_id();
    transport.request_position(request_id)?;
    let generation = io.sync_generation;
    let seek_epoch = io.seek_epoch;
    let position_revision = state.revision(TRANSPORT_POSITION_PATH);
    io.pending.insert(
        request_id,
        RequestKind::PositionPoll {
            generation,
            seek_epoch,
            position_revision,
        },
    );
    Ok(())
}

/// Issue a `transport.position` read every [`POSITION_POLL_INTERVAL`] while
/// connected + ready, never stacking a second poll behind an in-flight one.
fn poll_transport_position(
    mut link: ResMut<MixerTransportRes>,
    state: Res<MusicdMixerState>,
    mut io: ResMut<MixerIo>,
    mut last_poll: Local<Option<Instant>>,
) {
    if state.connection != MixerConnectionState::Connected || !state.ready {
        return;
    }
    if last_poll.is_some_and(|at| at.elapsed() < POSITION_POLL_INTERVAL) {
        return;
    }
    if io
        .pending
        .values()
        .any(|kind| matches!(kind, RequestKind::PositionPoll { .. }))
    {
        return;
    }
    // Pace on the attempt, not the outcome, so a transient send failure can't
    // hot-loop the poll.
    *last_poll = Some(Instant::now());
    let _ = request_position(link.0.as_mut(), &state, &mut io);
}

/// Drive the scrubber value + `M:SS / M:SS` readout from the extrapolated
/// clock. The write-back is suppressed while the scrubber is dragged (the
/// gesture owns the thumb) or has an in-flight seek (its own echo must not
/// fight it), and until the first poll lands (the ~10 Hz changed topic drives
/// it meanwhile). The scrubber's domain range follows `transport.length` as the
/// song loads.
#[allow(clippy::too_many_arguments)] // The follow system reads several small state sources.
fn update_transport_position(
    transport: Res<TransportPosition>,
    state: Res<MusicdMixerState>,
    io: Res<MixerIo>,
    scrubbers: Query<(Entity, &ControlValue), With<TransportScrubber>>,
    mut readouts: Query<&mut Text, With<TransportTimeReadout>>,
    mut last_length: Local<Option<f32>>,
    mut commands: Commands,
) {
    let length = transport_length_secs(&state);
    let playing = transport_is_playing(&state);
    let elapsed = transport
        .base_at
        .map(|at| at.elapsed().as_secs_f64())
        .unwrap_or(0.0);
    let live =
        extrapolate_position_seconds(transport.base_seconds, elapsed, playing, length as f64);
    let scrubber = scrubbers.iter().next();

    // Re-scale the scrubber travel when the song length changes.
    if *last_length != Some(length) {
        *last_length = Some(length);
        if let Some((scrubber, _)) = scrubber {
            let (range, mapping) = scrubber_range_for(length);
            commands.entity(scrubber).insert((range, mapping));
        }
    }

    // Programmatic follow — SetControlValue is the non-emitting write path.
    // The deviation reference is the WIDGET's actual value, not a memory of
    // our own last write: any stray thumb write from another code path then
    // self-heals within a frame (playing or stopped), while an idle,
    // in-sync transport still writes nothing.
    let live_f32 = live as f32;
    if transport.base_at.is_some()
        && !io.active_gestures.contains_key(TRANSPORT_POSITION_PATH)
        && !io.inflight_paths.contains(TRANSPORT_POSITION_PATH)
        && !io.queued_latest.contains_key(TRANSPORT_POSITION_PATH)
    {
        if let Some((scrubber, current)) = scrubber {
            if (current.0 - live_f32).abs() > 0.015 {
                commands.trigger(SetControlValue {
                    source: scrubber,
                    value: live_f32,
                });
            }
        }
    }

    if readouts.is_empty() {
        return;
    }
    // Before the first poll, show the last authoritative changed value.
    let shown = if transport.base_at.is_some() {
        live
    } else {
        leaf_number(state.value(TRANSPORT_POSITION_PATH)).unwrap_or(0.0)
    };
    let text = format!("{} / {}", format_mmss(shown), format_mmss(length as f64));
    for mut readout in &mut readouts {
        if readout.0 != text {
            readout.0.clone_from(&text);
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ChannelStripEntities {
    pub root: Entity,
    pub trim: Entity,
    pub pan: Entity,
    pub fader: Entity,
    pub meter: Entity,
    pub mute: Entity,
    pub solo: Entity,
}

/// The pixel geometry + typography of one channel strip. [`Default`] reproduces
/// the original 176px strip (TRIM/PAN captions, full MUTE/SOLO buttons, big
/// name); [`StripStyle::compact`] is the skinny 56px board strip (knob value
/// readouts, single-letter M/S buttons, a wrapped 2-line name). The two layouts
/// are selected by [`StripStyle::is_compact`] rather than a separate flag so the
/// struct stays purely descriptive.
#[derive(Clone, Copy, Debug)]
pub struct StripStyle {
    pub width: f32,
    pub knob: f32,
    pub fader_width: f32,
    pub fader_height: f32,
    pub meter_width: f32,
    pub name_font: f32,
    pub readout_font: f32,
    pub button_height: f32,
}

impl Default for StripStyle {
    fn default() -> Self {
        Self {
            width: 176.0,
            knob: 58.0,
            fader_width: 42.0,
            fader_height: 250.0,
            meter_width: 28.0,
            name_font: 18.0,
            readout_font: 13.0,
            button_height: 26.0,
        }
    }
}

impl StripStyle {
    /// The dense board strip: 32 of these plus a master fit side by side.
    pub fn compact() -> Self {
        Self {
            // One knob-column wide (TRIM stacked over PAN, M over S) — the
            // disp-skia strip proportions; 32 strips + master fit ~1650px.
            width: 44.0,
            knob: 26.0,
            fader_width: 12.0,
            fader_height: 300.0,
            meter_width: 12.0,
            name_font: 10.0,
            readout_font: 10.0,
            button_height: 18.0,
        }
    }

    /// A strip this narrow uses the compact layout — knob value readouts and
    /// single-letter M/S buttons instead of TRIM/PAN captions and full labels.
    /// The 176px default is wide; the 56px board strip is not.
    pub fn is_compact(&self) -> bool {
        self.width <= COMPACT_STRIP_MAX_WIDTH
    }
}

/// The width at or below which [`StripStyle::is_compact`] switches layouts.
/// Between the 56px board strip and the 176px default with margin to spare.
const COMPACT_STRIP_MAX_WIDTH: f32 = 96.0;

/// Spawn one channel strip at the default 176px style.
pub fn spawn_channel_strip(commands: &mut Commands, channel: usize) -> ChannelStripEntities {
    spawn_channel_strip_styled(commands, channel, &StripStyle::default())
}

/// Let a fader/meter follow its (flex-growing) row's height instead of its
/// spawn-time pixel height — the widget internals are percent-based, so the
/// live height is free. Surgical `Node.height` edit; every other field the
/// widget constructor chose is preserved.
fn stretch_to_row_height(commands: &mut Commands, entity: Entity) {
    commands
        .entity(entity)
        .entry::<Node>()
        .and_modify(|mut node| {
            node.height = percent(100);
        });
}

/// Spawn one channel strip sized + laid out per `style`. The returned
/// [`ChannelStripEntities`] are identical across styles; only the geometry and
/// the caption-vs-readout presentation differ (see [`StripStyle::is_compact`]).
pub fn spawn_channel_strip_styled(
    commands: &mut Commands,
    channel: usize,
    style: &StripStyle,
) -> ChannelStripEntities {
    assert!(channel < NUM_CHANNELS, "mixer channel is out of range");
    let base = format!("mixer.channels.{channel}");
    let compact = style.is_compact();

    let trim = commands
        .spawn((
            knob_sized(
                NumericControlProps::new(
                    format!("channel-{channel}-trim"),
                    0.0,
                    ControlRange {
                        min: -18.0,
                        max: 18.0,
                        step: 0.1,
                        detent: Some(0.0),
                    },
                    ValueMapping::linear(-18.0, 18.0).unwrap(),
                ),
                style.knob,
            ),
            MixerBinding::number(format!("{base}.trim")),
            ControlMeta::unit("dB"),
        ))
        .id();
    let pan = commands
        .spawn((
            knob_sized(
                NumericControlProps::new(
                    format!("channel-{channel}-pan"),
                    0.0,
                    ControlRange {
                        min: -1.0,
                        max: 1.0,
                        step: 1.0 / 512.0,
                        detent: Some(0.0),
                    },
                    ValueMapping::linear(-1.0, 1.0).unwrap(),
                ),
                style.knob,
            ),
            MixerBinding::number(format!("{base}.pan")),
        ))
        .id();
    let fader_entity = commands
        .spawn((
            fader_sized(
                NumericControlProps::new(
                    format!("channel-{channel}-fader"),
                    0.0,
                    ControlRange {
                        min: FADER_MIN_DB as f32,
                        max: FADER_MAX_DB as f32,
                        step: 0.1,
                        detent: Some(0.0),
                    },
                    default_fader_mapping(),
                ),
                style.fader_width,
                style.fader_height,
            ),
            MixerBinding::number(format!("{base}.fader")),
            ControlMeta::unit("dB"),
        ))
        .id();
    let meter = commands
        .spawn((
            level_meter_sized(
                format!("channel-{channel}-meter"),
                MeterValue::default(),
                style.meter_width,
                style.fader_height,
            ),
            MixerMeterBinding(channel),
        ))
        .id();

    let (mute_label, solo_label) = if compact {
        ("M", "S")
    } else {
        ("MUTE", "SOLO")
    };
    let button_min_w = if compact { 22.0 } else { 48.0 };
    let button_font = if compact { style.readout_font } else { 11.0 };
    let mute = commands
        .spawn((
            toggle_button_sized(
                format!("channel-{channel}-mute"),
                button_min_w,
                style.button_height,
            ),
            MixerBinding::boolean(format!("{base}.mute")),
        ))
        .with_child(control_text(
            mute_label,
            button_font,
            crate::theme::tokens::TEXT,
        ))
        .id();
    let solo = commands
        .spawn((
            toggle_button_sized(
                format!("channel-{channel}-solo"),
                button_min_w,
                style.button_height,
            ),
            MixerBinding::boolean(format!("{base}.solo")),
        ))
        .with_child(control_text(
            solo_label,
            button_font,
            crate::theme::tokens::TEXT,
        ))
        .id();

    let header = spawn_strip_header(commands, style, channel, &base);
    let trim_column = knob_column(commands, style, "TRIM", trim, compact.then_some(1));
    let pan_column = knob_column(commands, style, "PAN", pan, compact.then_some(2));
    // Compact strips stack TRIM above PAN (the disp-skia layout) so the strip
    // is only ONE knob-column wide; the full-size strip keeps them side by side.
    let knob_row = commands
        .spawn((Node {
            flex_direction: if compact {
                FlexDirection::Column
            } else {
                FlexDirection::Row
            },
            column_gap: px(if compact { 0.0 } else { 14.0 }),
            row_gap: px(if compact { 4.0 } else { 0.0 }),
            align_items: AlignItems::Center,
            justify_content: JustifyContent::Center,
            ..default()
        },))
        .add_children(&[trim_column, pan_column])
        .id();
    let fader_row = commands
        .spawn((Node {
            flex_direction: FlexDirection::Row,
            column_gap: px(if compact { 4.0 } else { 10.0 }),
            align_items: AlignItems::End,
            justify_content: JustifyContent::Center,
            // Compact board strips absorb the window's spare vertical space
            // (the fader/meter internals are all percent-based, so any
            // height works); the wide single-strip layout keeps its fixed
            // geometry.
            flex_grow: if compact { 1.0 } else { 0.0 },
            min_height: px(style.fader_height),
            ..default()
        },))
        .add_children(&[meter, fader_entity])
        .id();
    if compact {
        stretch_to_row_height(commands, meter);
        stretch_to_row_height(commands, fader_entity);
    }
    let readout = commands
        .spawn((
            control_text("0.0 dB", style.readout_font, crate::theme::tokens::TEXT_DIM),
            MixerReadout {
                control: fader_entity,
                precision: 1,
                suffix: " dB",
            },
        ))
        .id();
    // Compact strips likewise stack MUTE above SOLO at the strip's foot.
    let buttons = commands
        .spawn((Node {
            flex_direction: if compact {
                FlexDirection::Column
            } else {
                FlexDirection::Row
            },
            column_gap: px(if compact { 0.0 } else { 8.0 }),
            row_gap: px(if compact { 3.0 } else { 0.0 }),
            align_items: AlignItems::Center,
            justify_content: JustifyContent::Center,
            ..default()
        },))
        .add_children(&[mute, solo])
        .id();
    let root = commands
        .spawn((
            Node {
                width: px(style.width),
                min_height: px(if compact { 0.0 } else { 500.0 }),
                flex_direction: FlexDirection::Column,
                align_items: AlignItems::Center,
                row_gap: px(if compact { 5.0 } else { 10.0 }),
                padding: UiRect::all(px(if compact { 3.0 } else { 14.0 })),
                ..default()
            },
            bevy::feathers::theme::ThemeBackgroundColor(crate::theme::tokens::PANEL),
        ))
        .add_children(&[header, knob_row, fader_row, readout, buttons])
        .id();

    ChannelStripEntities {
        root,
        trim,
        pan,
        fader: fader_entity,
        meter,
        mute,
        solo,
    }
}

/// The strip header. Compact: a channel number over a wrapped 2-line name;
/// default: the single large name label. Both bind [`MixerName`] so the loaded
/// `mixer.channels.N.name` replaces the `Ch N` fallback.
fn spawn_strip_header(
    commands: &mut Commands,
    style: &StripStyle,
    channel: usize,
    base: &str,
) -> Entity {
    let compact = style.is_compact();
    // Compact strips already show the channel number on the line above, so a
    // channel with no loaded instrument name stays BLANK — a "Ch N" echo per
    // strip is clutter. The wide single-strip layout keeps the readable
    // fallback (it has no separate number line).
    let fallback_name = if compact {
        String::new()
    } else {
        format!("Ch {}", channel + 1)
    };
    let name = commands
        .spawn((
            control_text(
                fallback_name.clone(),
                style.name_font,
                crate::theme::tokens::TEXT,
            ),
            MixerName {
                path: format!("{base}.name"),
                fallback: fallback_name,
                split_lines: compact,
            },
        ))
        .id();
    if !compact {
        return name;
    }
    // Compact names are explicit word-split lines (split_name_lines), each
    // horizontally centered — never glyph wrap. The fixed-height box pins the
    // strip layout and vertically centers a one-line name.
    commands.entity(name).insert((
        Node {
            max_width: px(style.width - 6.0),
            ..default()
        },
        bevy::text::TextLayout {
            justify: bevy::text::Justify::Center,
            ..default()
        },
    ));
    let name_box = commands
        .spawn((Node {
            // Pinned box: the WIDTH clamp keeps a too-long line from
            // widening the box and bleeding across the neighbouring strip
            // (the text is trimmed to fit, but the clip is the backstop).
            width: px(style.width - 6.0),
            height: px(style.name_font * 2.0 + 4.0),
            flex_direction: FlexDirection::Column,
            align_items: AlignItems::Center,
            justify_content: JustifyContent::Center,
            overflow: bevy::ui::Overflow::clip(),
            ..default()
        },))
        .add_child(name)
        .id();
    let name = name_box;
    let number = commands
        .spawn(control_text(
            format!("{}", channel + 1),
            style.readout_font,
            crate::theme::tokens::TEXT_DIM,
        ))
        .id();
    commands
        .spawn((Node {
            flex_direction: FlexDirection::Column,
            align_items: AlignItems::Center,
            row_gap: px(1),
            ..default()
        },))
        .add_children(&[number, name])
        .id()
}

/// A knob with its caption, and — when `readout_precision` is set (compact
/// mode) — a live numeric value readout below it. In the default style the
/// caption sits above the bare knob, matching the original `labelled_control`.
fn knob_column(
    commands: &mut Commands,
    style: &StripStyle,
    caption: &str,
    knob: Entity,
    readout_precision: Option<usize>,
) -> Entity {
    let compact = style.is_compact();
    let caption_font = if compact {
        (style.readout_font - 1.0).max(7.0)
    } else {
        11.0
    };
    let caption_entity = commands
        .spawn(control_text(
            caption,
            caption_font,
            crate::theme::tokens::TEXT_DIM,
        ))
        .id();
    let mut children = vec![caption_entity, knob];
    if let Some(precision) = readout_precision {
        let readout = commands
            .spawn((
                control_text("0.0", style.readout_font, crate::theme::tokens::TEXT_DIM),
                MixerReadout {
                    control: knob,
                    precision,
                    suffix: "",
                },
            ))
            .id();
        children.push(readout);
    }
    commands
        .spawn((Node {
            flex_direction: FlexDirection::Column,
            align_items: AlignItems::Center,
            row_gap: px(if compact { 3.0 } else { 5.0 }),
            ..default()
        },))
        .add_children(&children)
        .id()
}

fn control_text(
    text: impl Into<String>,
    size: f32,
    token: bevy::feathers::theme::ThemeToken,
) -> impl bevy::prelude::Bundle {
    (
        Text::new(text),
        TextFont::from_font_size(size),
        bevy::feathers::theme::ThemeTextColor(token),
        Pickable::IGNORE,
    )
}

/// Spawn the master strip: a fader (`mixer.master.fader`), the master meter
/// (record [`NUM_CHANNELS`]), a dB readout, and a single mute — no trim/pan and
/// no solo (the master has neither). Returns the strip root entity.
pub fn spawn_master_strip(commands: &mut Commands, style: &StripStyle) -> Entity {
    let compact = style.is_compact();
    let fader_entity = commands
        .spawn((
            fader_sized(
                NumericControlProps::new(
                    "master-fader",
                    0.0,
                    ControlRange {
                        min: FADER_MIN_DB as f32,
                        max: FADER_MAX_DB as f32,
                        step: 0.1,
                        detent: Some(0.0),
                    },
                    default_fader_mapping(),
                ),
                style.fader_width,
                style.fader_height,
            ),
            MixerBinding::number("mixer.master.fader"),
            ControlMeta::unit("dB"),
        ))
        .id();
    let meter = commands
        .spawn((
            level_meter_sized(
                "master-meter",
                MeterValue::default(),
                style.meter_width,
                style.fader_height,
            ),
            MixerMeterBinding(NUM_CHANNELS),
        ))
        .id();
    let mute_label = if compact { "M" } else { "MUTE" };
    let button_min_w = if compact { 22.0 } else { 48.0 };
    let button_font = if compact { style.readout_font } else { 11.0 };
    let mute = commands
        .spawn((
            toggle_button_sized("master-mute", button_min_w, style.button_height),
            MixerBinding::boolean("mixer.master.mute"),
        ))
        .with_child(control_text(
            mute_label,
            button_font,
            crate::theme::tokens::TEXT,
        ))
        .id();

    // Compact master mirrors the channel strip's exact vertical skeleton —
    // same header box (number line + pinned two-line name) and a HIDDEN copy
    // of the stacked knob block — so the master fader row aligns with the
    // channel fader rows by construction, not by hand-tuned spacing.
    let label = if compact {
        let text = commands
            .spawn(control_text(
                "Master",
                style.name_font,
                crate::theme::tokens::TEXT,
            ))
            .id();
        commands.entity(text).insert((
            Node {
                max_width: px(style.width - 6.0),
                ..default()
            },
            bevy::text::TextLayout {
                justify: bevy::text::Justify::Center,
                ..default()
            },
        ));
        // Same centering wrapper as the channel header (see
        // spawn_strip_header) so "Master" aligns with the channel names.
        let name = commands
            .spawn((Node {
                width: px(style.width - 6.0),
                height: px(style.name_font * 2.0 + 4.0),
                flex_direction: FlexDirection::Column,
                align_items: AlignItems::Center,
                justify_content: JustifyContent::Center,
                overflow: bevy::ui::Overflow::clip(),
                ..default()
            },))
            .add_child(text)
            .id();
        let number_slot = commands
            .spawn(control_text(
                " ",
                style.readout_font,
                crate::theme::tokens::TEXT_DIM,
            ))
            .id();
        commands
            .spawn((Node {
                flex_direction: FlexDirection::Column,
                align_items: AlignItems::Center,
                row_gap: px(1),
                ..default()
            },))
            .add_children(&[number_slot, name])
            .id()
    } else {
        commands
            .spawn(control_text(
                "Master",
                style.name_font,
                crate::theme::tokens::TEXT,
            ))
            .id()
    };
    let knob_spacer = compact.then(|| {
        let dummy_knob = |commands: &mut Commands, id: &str| {
            let entity = commands
                .spawn((
                    knob_sized(
                        NumericControlProps::new(
                            id,
                            0.0,
                            ControlRange {
                                min: -1.0,
                                max: 1.0,
                                step: 0.1,
                                detent: Some(0.0),
                            },
                            ValueMapping::linear(-1.0, 1.0).expect("spacer mapping is valid"),
                        ),
                        style.knob,
                    ),
                    bevy::ui::InteractionDisabled,
                ))
                .id();
            // Tab navigation does not skip disabled/hidden controls — an
            // invisible spacer must never take keyboard focus. Likewise the
            // app-control registry does not skip disabled widgets, so mark
            // the spacer neither queryable nor writable (never registered).
            commands
                .entity(entity)
                .insert(BusWidget {
                    id: id.to_string(),
                    queryable: false,
                    writable: false,
                })
                .remove::<bevy::input_focus::tab_navigation::TabIndex>();
            entity
        };
        let trim_slot = dummy_knob(commands, "master-spacer-trim");
        let pan_slot = dummy_knob(commands, "master-spacer-pan");
        let trim_column = knob_column(commands, style, "TRIM", trim_slot, Some(1));
        let pan_column = knob_column(commands, style, "PAN", pan_slot, Some(2));
        commands
            .spawn((
                Node {
                    flex_direction: FlexDirection::Column,
                    row_gap: px(4),
                    align_items: AlignItems::Center,
                    ..default()
                },
                // Hidden keeps its layout space (unlike Display::None) — the
                // whole point of the spacer.
                bevy::camera::visibility::Visibility::Hidden,
            ))
            .add_children(&[trim_column, pan_column])
            .id()
    });
    let fader_row = commands
        .spawn((Node {
            flex_direction: FlexDirection::Row,
            column_gap: px(if compact { 4.0 } else { 10.0 }),
            align_items: AlignItems::End,
            justify_content: JustifyContent::Center,
            // Mirror the channel strips' vertical stretch (see
            // spawn_channel_strip_styled) so the master fader spans the same
            // travel; the wide layout keeps fixed geometry.
            flex_grow: if compact { 1.0 } else { 0.0 },
            min_height: px(style.fader_height),
            ..default()
        },))
        .add_children(&[meter, fader_entity])
        .id();
    if compact {
        stretch_to_row_height(commands, meter);
        stretch_to_row_height(commands, fader_entity);
    }
    let readout = commands
        .spawn((
            control_text("0.0 dB", style.readout_font, crate::theme::tokens::TEXT_DIM),
            MixerReadout {
                control: fader_entity,
                precision: 1,
                suffix: " dB",
            },
        ))
        .id();
    commands
        .spawn((
            Node {
                width: px(style.width),
                min_height: px(if compact { 0.0 } else { 500.0 }),
                flex_direction: FlexDirection::Column,
                align_items: AlignItems::Center,
                row_gap: px(if compact { 5.0 } else { 10.0 }),
                padding: UiRect::all(px(if compact { 3.0 } else { 14.0 })),
                ..default()
            },
            // A faintly warmer panel distinguishes the master from the channels.
            bevy::feathers::theme::ThemeBackgroundColor(crate::theme::tokens::MASTER_PANEL),
        ))
        .add_children(&match knob_spacer {
            Some(spacer) => vec![label, spacer, fader_row, readout, mute],
            None => vec![label, fader_row, readout, mute],
        })
        .id()
}

/// The entities of a transport footer, for parenting + wiring.
#[derive(Clone, Copy, Debug)]
pub struct TransportFooter {
    pub root: Entity,
    pub play: Entity,
    pub stop: Entity,
    pub rtz: Entity,
    pub scrubber: Entity,
    pub time: Entity,
}

/// Spawn the transport footer: RTZ / Play / Stop momentary buttons, a
/// horizontal position scrubber (`scrubber_width` px wide) bound to
/// `transport.position`, and a `M:SS / M:SS` time readout. Play/Stop write
/// `transport.state`; RTZ seeks to 0; dragging the scrubber streams live seeks.
/// The scrubber range + time are updated live by [`update_transport_position`].
pub fn spawn_transport_footer(
    commands: &mut Commands,
    style: &StripStyle,
    scrubber_width: f32,
) -> TransportFooter {
    let button_font = style.readout_font + 1.0;
    let rtz = commands
        .spawn((
            action_button("transport-rtz", 46.0, 26.0),
            // A zero-valued number write seeks to the start.
            MixerBinding::number(TRANSPORT_POSITION_PATH),
            ControlMeta::action("transport.position=0"),
        ))
        .with_child(control_text("RTZ", button_font, crate::theme::tokens::TEXT))
        .id();
    let play = commands
        .spawn((
            action_button("transport-play", 56.0, 26.0),
            MixerBinding::enum_write(TRANSPORT_STATE_PATH, "playing"),
            ControlMeta::action("transport.state=playing"),
        ))
        .with_child(control_text(
            "Play",
            button_font,
            crate::theme::tokens::TEXT,
        ))
        .id();
    let stop = commands
        .spawn((
            action_button("transport-stop", 56.0, 26.0),
            MixerBinding::enum_write(TRANSPORT_STATE_PATH, "stopped"),
            ControlMeta::action("transport.state=stopped"),
        ))
        .with_child(control_text(
            "Stop",
            button_font,
            crate::theme::tokens::TEXT,
        ))
        .id();
    let (range, mapping) = scrubber_range_for(0.0);
    let scrubber = commands
        .spawn((
            hfader_sized(
                NumericControlProps::new("transport-position", 0.0, range, mapping),
                scrubber_width,
                24.0,
            ),
            MixerBinding::number(TRANSPORT_POSITION_PATH),
            TransportScrubber,
            ControlMeta {
                kind: Some("scrubber".into()),
                ..ControlMeta::unit("s")
            },
        ))
        .id();
    // The scrubber FLEXES to the remaining footer width instead of trusting
    // `scrubber_width` (display scaling can clamp the window narrower than
    // the requested resolution — a fixed track then runs off the right
    // edge). hfader visuals + pointer mapping already read the live
    // ComputedNode width, so shrink/grow costs nothing. This replaces the
    // hfader's own Node: same shape, flexible main size.
    commands.entity(scrubber).insert(Node {
        width: px(scrubber_width),
        min_width: px(200),
        flex_grow: 1.0,
        flex_shrink: 1.0,
        height: px(24),
        position_type: bevy::ui::PositionType::Relative,
        justify_content: JustifyContent::Center,
        align_items: AlignItems::Center,
        border_radius: bevy::ui::BorderRadius::all(px(5)),
        ..default()
    });
    let time = commands
        .spawn((
            control_text(
                "0:00 / 0:00",
                style.readout_font + 3.0,
                crate::theme::tokens::TEXT,
            ),
            TransportTimeReadout,
            // Right of the flexible scrubber: the scrubber absorbs all width
            // pressure, and shrink-0 keeps the readout at content size so it
            // can never be squeezed into wrapping.
            Node {
                flex_shrink: 0.0,
                ..default()
            },
        ))
        .id();
    let root = commands
        .spawn((
            Node {
                // Full-width row so the flexible scrubber sizes to the real
                // window, whatever the display scale clamps it to.
                width: percent(100),
                flex_direction: FlexDirection::Row,
                align_items: AlignItems::Center,
                column_gap: px(12),
                padding: UiRect::all(px(10)),
                ..default()
            },
            bevy::feathers::theme::ThemeBackgroundColor(crate::theme::tokens::PANEL),
        ))
        // Time sits RIGHT of the scrubber. Safe now that the scrubber
        // flex-shrinks (it was the fixed-width track that used to push the
        // readout off-window); the readout itself is shrink-0 so it cannot
        // wrap.
        .add_children(&[rtz, play, stop, scrubber, time])
        .id();
    TransportFooter {
        root,
        play,
        stop,
        rtz,
        scrubber,
        time,
    }
}

/// Spawn the song-metadata footer: `title · artist · copyright`, each bound to
/// its `mixer.song.*` leaf (populated from the snapshot; the fallback shows
/// until then). Returns the footer root entity.
pub fn spawn_song_footer(commands: &mut Commands) -> Entity {
    let sep = |commands: &mut Commands| {
        commands
            .spawn(control_text("  -  ", 13.0, crate::theme::tokens::TEXT_DIM))
            .id()
    };
    let title = song_label(commands, "mixer.song.title", "Untitled");
    let sep1 = sep(commands);
    let artist = song_label(commands, "mixer.song.artist", "Unknown artist");
    let sep2 = sep(commands);
    let copyright = song_label(commands, "mixer.song.copyright", "");
    commands
        .spawn((
            Node {
                // Full-width so justify-center actually centers on screen
                // (a content-sized row just sits at the parent's left edge).
                width: percent(100),
                flex_direction: FlexDirection::Row,
                align_items: AlignItems::Center,
                justify_content: JustifyContent::Center,
                padding: UiRect::all(px(6)),
                ..default()
            },
            bevy::feathers::theme::ThemeBackgroundColor(crate::theme::tokens::SURFACE),
        ))
        .add_children(&[title, sep1, artist, sep2, copyright])
        .id()
}

fn song_label(commands: &mut Commands, path: &str, fallback: &str) -> Entity {
    commands
        .spawn((
            control_text(fallback, 13.0, crate::theme::tokens::TEXT_DIM),
            MixerName {
                path: path.to_string(),
                fallback: fallback.to_string(),
                // Footer labels are single-line prose, never split.
                split_lines: false,
            },
        ))
        .id()
}

/// The P6/D1 measurement harness, shared verbatim by every arm that runs the
/// board (`--smoke-write` / `--smoke-stream`): a 10-second ~60 Hz sine
/// gesture stream on a chosen fader through the REAL gesture path
/// (`ValueChange`, `is_final = false`) plus a tracked final commit, exiting
/// only once the commit is accepted AND `dsp.applied` covers it. Kept
/// in-tree (and in ctk, not per-binary) so the split and fused bench arms
/// cannot drift apart in workload. Apps opt in by inserting [`SmokeRun`] /
/// [`StreamRun`] with the target fader entity; the systems are registered by
/// [`MusicdMixerPlugin`] and are inert without the resources.
pub mod smoke {
    use bevy::app::AppExit;
    use bevy::ecs::entity::Entity;
    use bevy::ecs::resource::Resource;
    use bevy::ecs::system::{Commands, Res, ResMut};
    use bevy::log::info;
    use bevy::ui_widgets::ValueChange;

    use super::{default_fader_mapping, MixerConnectionState, MusicdMixerState};

    /// One-shot final-commit smoke (`--smoke-write`): set channel-0's fader
    /// to −6 dB and exit once the write round-trips AND the DSP latched it.
    #[derive(Resource)]
    pub struct SmokeRun {
        pub fader: Entity,
        pub phase: SmokePhase,
    }

    impl SmokeRun {
        pub fn new(fader: Entity) -> Self {
            Self {
                fader,
                phase: SmokePhase::Waiting,
            }
        }
    }

    pub enum SmokePhase {
        Waiting,
        Awaiting {
            initial_revision: u64,
            deadline: std::time::Instant,
        },
    }

    /// The 10-second gesture-stream benchmark (`--smoke-stream`).
    #[derive(Resource)]
    pub struct StreamRun {
        pub fader: Entity,
        pub phase: StreamPhase,
    }

    impl StreamRun {
        pub fn new(fader: Entity) -> Self {
            Self {
                fader,
                phase: StreamPhase::Waiting,
            }
        }
    }

    pub enum StreamPhase {
        Waiting,
        Streaming {
            start: std::time::Instant,
            frames: u64,
        },
        /// The final commit is in flight: exit only once it is accepted AND
        /// `dsp.applied` covers it (or the deadline panics) — exiting in the
        /// commit's frame can abort the write and strand the fader mid-sweep.
        AwaitingCommit {
            initial_revision: u64,
            frames: u64,
            deadline: std::time::Instant,
        },
    }

    pub fn smoke_write(
        mut commands: Commands,
        state: Res<MusicdMixerState>,
        run: Option<ResMut<SmokeRun>>,
    ) {
        let Some(mut run) = run else { return };
        let path = "mixer.channels.0.fader";
        match run.phase {
            SmokePhase::Waiting => {
                let Some(initial_revision) = state.revision(path) else {
                    return;
                };
                if state.connection != MixerConnectionState::Connected || !state.ready {
                    return;
                }
                commands.trigger(ValueChange {
                    source: run.fader,
                    value: default_fader_mapping().to_position(-6.0),
                    is_final: true,
                });
                run.phase = SmokePhase::Awaiting {
                    initial_revision,
                    deadline: std::time::Instant::now() + std::time::Duration::from_secs(10),
                };
            }
            SmokePhase::Awaiting {
                initial_revision,
                deadline,
            } => {
                let revision = state.revision(path).unwrap_or(initial_revision);
                let accepted = revision > initial_revision
                    && state.value(path) == Some(&cosmix_mixer_schema::LeafValue::Number(-6.0));
                let applied = state
                    .last_applied_revision
                    .is_some_and(|applied| applied >= revision);
                if accepted && applied {
                    info!(
                        revision,
                        "CTK_SMOKE_OK: final gesture reached musicd and DSP"
                    );
                    commands.write_message(AppExit::Success);
                } else if std::time::Instant::now() >= deadline {
                    panic!("CTK smoke write timed out: accepted={accepted}, applied={applied}");
                }
            }
        }
    }

    /// Drive the P6 measurement stream: once connected+ready, issue a
    /// throttled `is_final = false` gesture every frame for 10 seconds (the
    /// `ctk-latency` report prints every 5s), then commit a final gesture and
    /// exit. `frames` counts stream frames; the achieved writes/second is
    /// read from the `ctk-latency` issue→ack `n` over the elapsed window.
    pub fn smoke_stream(
        mut commands: Commands,
        state: Res<MusicdMixerState>,
        run: Option<ResMut<StreamRun>>,
    ) {
        let Some(mut run) = run else { return };
        let fader = run.fader;
        match &mut run.phase {
            StreamPhase::Waiting => {
                if state.connection == MixerConnectionState::Connected && state.ready {
                    run.phase = StreamPhase::Streaming {
                        start: std::time::Instant::now(),
                        frames: 0,
                    };
                }
            }
            StreamPhase::Streaming { start, frames } => {
                let elapsed = start.elapsed();
                if elapsed.as_secs_f64() >= 10.0 {
                    let frames = *frames;
                    let initial_revision = state.revision("mixer.channels.0.fader").unwrap_or(0);
                    commands.trigger(ValueChange {
                        source: fader,
                        value: default_fader_mapping().to_position(0.0),
                        is_final: true,
                    });
                    info!(
                        frames,
                        elapsed_ms = elapsed.as_millis() as u64,
                        "CTK_STREAM_DONE"
                    );
                    run.phase = StreamPhase::AwaitingCommit {
                        initial_revision,
                        frames,
                        deadline: std::time::Instant::now() + std::time::Duration::from_secs(10),
                    };
                    return;
                }
                let secs = elapsed.as_secs_f64();
                let value = (0.5 + 0.4 * (secs * 2.5).sin()) as f32;
                commands.trigger(ValueChange {
                    source: fader,
                    value,
                    is_final: false,
                });
                *frames += 1;
            }
            StreamPhase::AwaitingCommit {
                initial_revision,
                frames,
                deadline,
            } => {
                let path = "mixer.channels.0.fader";
                let revision = state.revision(path).unwrap_or(*initial_revision);
                // Revision advancement alone could be an earlier in-flight
                // stream ack — require the authoritative value to BE the
                // final commit target (0.0 dB) as well.
                let accepted = revision > *initial_revision
                    && state.value(path) == Some(&cosmix_mixer_schema::LeafValue::Number(0.0));
                let applied = state
                    .last_applied_revision
                    .is_some_and(|applied| applied >= revision);
                if accepted && applied {
                    info!(frames = *frames, revision, "CTK_STREAM_COMMITTED");
                    commands.write_message(AppExit::Success);
                } else if std::time::Instant::now() >= *deadline {
                    panic!(
                        "stream smoke final commit timed out: accepted={accepted}, applied={applied}"
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::widgets::knob;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct RecordingTransportState {
        writes: Vec<(u64, WriteRequest)>,
        events: Vec<TransportEvent>,
        messages: Vec<TransportMessage>,
        local_issue_error: Option<String>,
    }

    struct RecordingTransport {
        state: Arc<Mutex<RecordingTransportState>>,
    }

    impl MixerTransport for RecordingTransport {
        fn service_name(&self) -> &str {
            "ctk-test"
        }

        fn issue_write(&mut self, request_id: u64, request: &WriteRequest) -> Result<(), String> {
            let mut state = self.state.lock().unwrap();
            state.writes.push((request_id, request.clone()));
            match &state.local_issue_error {
                Some(error) => Err(error.clone()),
                None => Ok(()),
            }
        }

        fn request_snapshot(&mut self, _request_id: u64) -> Result<(), String> {
            Ok(())
        }

        fn request_position(&mut self, _request_id: u64) -> Result<(), String> {
            Ok(())
        }

        fn poll_events(&mut self, out: &mut Vec<TransportEvent>) {
            out.append(&mut self.state.lock().unwrap().events);
        }

        fn poll_messages(&mut self, out: &mut Vec<TransportMessage>) {
            out.append(&mut self.state.lock().unwrap().messages);
        }

        fn discard_backlog(&mut self) {
            self.state.lock().unwrap().messages.clear();
        }
    }

    fn test_meta(generation: u64, gesture_id: Option<u64>, surface: &'static str) -> CommandMeta {
        CommandMeta {
            source: CommandSource::app(surface),
            gesture_id: gesture_id.map(GestureId),
            generation: CommandGeneration(generation),
        }
    }

    fn queued_number(value: f64, generation: u64, gesture_id: Option<u64>) -> QueuedWrite {
        QueuedWrite::new(
            LeafValue::Number(value),
            test_meta(generation, gesture_id, "test"),
        )
    }

    fn queued_enum(value: &str, generation: u64) -> QueuedWrite {
        QueuedWrite::new(
            LeafValue::Enum(value.to_string()),
            test_meta(generation, None, "test"),
        )
    }

    fn test_active_gesture(id: u64) -> ActiveGesture {
        ActiveGesture {
            id: GestureId(id),
            owner: Entity::PLACEHOLDER,
        }
    }

    fn issued_number(
        path: &str,
        value: f64,
        if_revision: Option<u64>,
        generation: u64,
        gesture_id: Option<u64>,
    ) -> IssuedWrite {
        IssuedWrite {
            request: WriteRequest {
                path: path.into(),
                value: LeafValue::Number(value),
                op_id: format!("test-op-{generation}"),
                if_revision,
            },
            meta: test_meta(generation, gesture_id, "test"),
        }
    }

    fn issued_enum(path: &str, value: &str, generation: u64) -> IssuedWrite {
        IssuedWrite {
            request: WriteRequest {
                path: path.into(),
                value: LeafValue::Enum(value.to_string()),
                op_id: format!("test-op-{generation}"),
                if_revision: Some(7),
            },
            meta: test_meta(generation, None, "test"),
        }
    }

    fn ready_transport_state(value: &str) -> MusicdMixerState {
        let mut state = MusicdMixerState {
            connection: MixerConnectionState::Connected,
            ready: true,
            ..default()
        };
        state.values.insert(
            TRANSPORT_STATE_PATH.into(),
            LeafValue::Enum(value.to_string()),
        );
        state.revisions.insert(TRANSPORT_STATE_PATH.into(), 7);
        state
    }

    fn transport_gesture_test_app() -> (App, Arc<Mutex<RecordingTransportState>>) {
        let transport_state = Arc::new(Mutex::new(RecordingTransportState::default()));
        let transport = RecordingTransport {
            state: Arc::clone(&transport_state),
        };
        let mut state = MusicdMixerState {
            connection: MixerConnectionState::Connected,
            ready: true,
            ..default()
        };
        state
            .values
            .insert(TRANSPORT_POSITION_PATH.into(), LeafValue::Number(4.0));
        state
            .values
            .insert(TRANSPORT_LENGTH_PATH.into(), LeafValue::Number(120.0));
        state.revisions.insert(TRANSPORT_POSITION_PATH.into(), 7);
        let transport_position = TransportPosition {
            base_seconds: 12.0,
            base_at: Some(Instant::now()),
            playing: false,
        };

        let mut app = App::new();
        app.insert_resource(MixerTransportRes(Box::new(transport)))
            .insert_resource(state)
            .insert_resource(transport_position)
            .init_resource::<MixerIo>()
            .add_observer(on_control_change)
            .add_observer(on_transport_seek_gesture);
        (app, transport_state)
    }

    #[test]
    fn meter_floor_and_zero_db_are_useful_positions() {
        assert_eq!(db_to_meter_position(-120.0), 0.0);
        assert_eq!(db_to_meter_position(-60.0), 0.0);
        assert!((db_to_meter_position(0.0) - 60.0 / 66.0).abs() < 1e-6);
        assert_eq!(db_to_meter_position(24.0), 1.0);
    }

    #[test]
    fn authoritative_state_rejects_only_older_revisions() {
        let mut state = MusicdMixerState::default();
        let path = "mixer.channels.0.fader".to_string();
        assert!(state.accept_leaf(path.clone(), LeafValue::Number(-12.0), 4));
        assert!(!state.accept_leaf(path.clone(), LeafValue::Number(-6.0), 3));
        assert!(state.accept_leaf(path.clone(), LeafValue::Number(-3.0), 4));
        assert_eq!(state.value(&path), Some(&LeafValue::Number(-3.0)));
    }

    #[test]
    fn transport_projection_prefers_queued_value_over_acknowledged() {
        let state = ready_transport_state("stopped");
        let mut io = MixerIo::default();
        io.queued_latest
            .insert(TRANSPORT_STATE_PATH.into(), queued_enum("playing", 1));

        assert_eq!(
            desired_transport(&state, Some(&io)),
            Some(DesiredTransport {
                playing: true,
                provisional: true,
            })
        );
    }

    #[test]
    fn transport_projection_prefers_inflight_value_over_acknowledged() {
        let state = ready_transport_state("stopped");
        let mut io = MixerIo::default();
        io.pending.insert(
            1,
            RequestKind::Write {
                write: issued_enum(TRANSPORT_STATE_PATH, "playing", 1),
                attempt: 0,
            },
        );

        assert_eq!(
            desired_transport(&state, Some(&io)),
            Some(DesiredTransport {
                playing: true,
                provisional: true,
            })
        );
    }

    #[test]
    fn transport_projection_prefers_retry_value_over_acknowledged() {
        let state = ready_transport_state("stopped");
        let mut io = MixerIo::default();
        io.retries.push(RetryWrite {
            due: Instant::now(),
            write: issued_enum(TRANSPORT_STATE_PATH, "playing", 1),
            attempt: 1,
        });

        assert_eq!(
            desired_transport(&state, Some(&io)),
            Some(DesiredTransport {
                playing: true,
                provisional: true,
            })
        );
    }

    #[test]
    fn transport_projection_falls_through_to_acknowledged_value() {
        let state = ready_transport_state("stopped");

        assert_eq!(
            desired_transport(&state, Some(&MixerIo::default())),
            Some(DesiredTransport {
                playing: false,
                provisional: false,
            })
        );
        assert_eq!(
            desired_transport(&state, None),
            Some(DesiredTransport {
                playing: false,
                provisional: false,
            })
        );

        let unknown = MusicdMixerState {
            connection: MixerConnectionState::Connected,
            ready: true,
            ..default()
        };
        assert_eq!(desired_transport(&unknown, None), None);
    }

    #[test]
    fn transport_projection_is_unknown_until_connected_and_ready() {
        let mut state = ready_transport_state("playing");
        state.connection = MixerConnectionState::Disconnected;
        assert_eq!(desired_transport(&state, Some(&MixerIo::default())), None);

        state.connection = MixerConnectionState::Connected;
        state.ready = false;
        assert_eq!(desired_transport(&state, Some(&MixerIo::default())), None);
    }

    #[test]
    fn position_rejection_rebases_below_the_stale_cas_token() {
        // The seek wedge: after a song-bank swap the store's transport.position
        // revision rolls BACK (39 -> 37) while the client still holds 39. The
        // monotonic guard refuses to lower it, and position is exempt from the
        // snapshot epoch-reset (begin_snapshot) — so `resync_leaf` is the only
        // recovery. Without it every future TransportSeekRequest re-rejects
        // with a stale if_revision=39 (ruler-drag + load-reset both wedge).
        let mut state = MusicdMixerState::default();
        let path = TRANSPORT_POSITION_PATH.to_string();
        assert!(state.accept_leaf(path.clone(), LeafValue::Number(12.0), 39));
        // The rolled-back authoritative revision is refused by the guard...
        assert!(!state.accept_leaf(path.clone(), LeafValue::Number(0.0), 37));
        assert_eq!(state.revision(&path), Some(39), "monotonic guard holds 39");
        // ...but the rejection's ground truth force-rebases the CAS token, so
        // the NEXT seek issues if_revision=37 and matches the store.
        state.resync_leaf(path.clone(), LeafValue::Number(0.0), 37);
        assert_eq!(state.revision(&path), Some(37));
        assert_eq!(state.value(&path), Some(&LeafValue::Number(0.0)));
        // A subsequent forward advance is accepted normally (convergence).
        assert!(state.accept_leaf(path.clone(), LeafValue::Number(0.0), 38));
        assert_eq!(state.revision(&path), Some(38));
    }

    fn first_issued_seek(mode: SubmitMode, gesture: Option<u64>) -> Option<WriteRequest> {
        let transport_state = Arc::new(Mutex::new(RecordingTransportState::default()));
        let mut res = MixerTransportRes(Box::new(RecordingTransport {
            state: Arc::clone(&transport_state),
        }));
        let mut state = MusicdMixerState {
            connection: MixerConnectionState::Connected,
            ready: true,
            ..default()
        };
        // A follower revision that has drifted above the store's (the mid-play
        // divergence that wedged the CAS-gated seek).
        state.revisions.insert(TRANSPORT_POSITION_PATH.into(), 39);
        let mut io = MixerIo::default();
        submit_write(
            Some(&mut res),
            &mut state,
            &mut io,
            TRANSPORT_POSITION_PATH.to_string(),
            queued_number(0.0, 1, gesture),
            mode,
        );
        let writes = transport_state.lock().unwrap().writes.clone();
        writes.first().map(|(_, request)| request.clone())
    }

    #[test]
    fn app_seek_to_transport_position_is_issued_without_a_cas_token() {
        // RTZ / stop rewind: a Discrete app-seek (no gesture) to
        // transport.position must issue if_revision=None. The follower's
        // per-path revision drifts from the store during playback, so a
        // CAS-gated seek is spuriously rejected and the engine never rewinds.
        let app_seek = first_issued_seek(SubmitMode::Discrete, None)
            .expect("a Discrete app-seek issues immediately");
        assert_eq!(app_seek.path, TRANSPORT_POSITION_PATH);
        assert_eq!(app_seek.if_revision, None, "app-seek must not be CAS-gated");

        // A gesture Commit seek to the same path keeps its CAS floor so
        // concurrent drag writes stay ordered.
        let gesture_commit = first_issued_seek(SubmitMode::Commit, Some(5))
            .expect("a Commit seek issues immediately");
        assert_eq!(
            gesture_commit.if_revision,
            Some(39),
            "gesture commit keeps CAS ordering"
        );
    }

    #[test]
    fn resync_is_scoped_ordinary_leaves_keep_the_monotonic_guard() {
        // The force-rebase in handle_write_reply is gated to transport.position;
        // ordinary leaves recover via the snapshot epoch-reset instead, so their
        // monotonic guard must still refuse a lower (possibly delayed) revision —
        // the invariant that stops a stale rejection rewinding newer state.
        let mut state = MusicdMixerState::default();
        let fader = "mixer.channels.0.fader".to_string();
        assert!(state.accept_leaf(fader.clone(), LeafValue::Number(-3.0), 40));
        assert!(!state.accept_leaf(fader.clone(), LeafValue::Number(-9.0), 37));
        assert_eq!(state.revision(&fader), Some(40));
        assert_eq!(state.value(&fader), Some(&LeafValue::Number(-3.0)));
    }

    #[test]
    fn authoritative_snapshot_starts_a_fresh_revision_epoch() {
        let mut state = MusicdMixerState::default();
        let path = "mixer.channels.0.fader".to_string();
        assert!(state.accept_leaf(path.clone(), LeafValue::Number(-12.0), 40));
        state.snapshot_revision = Some(40);

        let disposition = state.begin_snapshot(&MixerSnapshotResponse {
            revision: 1,
            real_audio: false,
            audio_fault: false,
            applied_fault: false,
            source_profile: String::new(),
            benchmark_eligible: false,
            leaves: Vec::new(),
        });

        assert_eq!(disposition, SnapshotDisposition::EpochReset);
        assert!(state.ready);
        assert!(state.accept_leaf(path.clone(), LeafValue::Number(-6.0), 1));
        assert_eq!(state.revision(&path), Some(1));
        assert_eq!(state.value(&path), Some(&LeafValue::Number(-6.0)));
    }

    #[test]
    fn periodic_snapshot_retains_a_newer_acknowledged_path_revision() {
        let mut state = MusicdMixerState {
            ready: true,
            snapshot_revision: Some(5),
            ..default()
        };
        let path = "mixer.channels.0.fader".to_string();
        assert!(state.accept_leaf(path.clone(), LeafValue::Number(0.5), 7));

        let disposition = state.begin_snapshot(&MixerSnapshotResponse {
            revision: 6,
            real_audio: false,
            audio_fault: false,
            applied_fault: false,
            source_profile: String::new(),
            benchmark_eligible: false,
            leaves: Vec::new(),
        });

        assert_eq!(disposition, SnapshotDisposition::ConfirmEpoch);
        assert_eq!(state.revision(&path), Some(7));
        assert_eq!(state.value(&path), Some(&LeafValue::Number(0.5)));

        let caught_up = state.begin_snapshot(&MixerSnapshotResponse {
            revision: 7,
            real_audio: false,
            audio_fault: false,
            applied_fault: false,
            source_profile: String::new(),
            benchmark_eligible: false,
            leaves: Vec::new(),
        });
        assert_eq!(caught_up, SnapshotDisposition::Applied);
    }

    #[test]
    fn two_lagging_snapshots_confirm_an_in_place_revision_epoch() {
        let mut state = MusicdMixerState {
            ready: true,
            snapshot_revision: Some(5),
            ..default()
        };
        let path = "mixer.channels.0.fader".to_string();
        assert!(state.accept_leaf(path.clone(), LeafValue::Number(-6.0), 40));

        let snapshot = MixerSnapshotResponse {
            revision: 6,
            real_audio: false,
            audio_fault: false,
            applied_fault: false,
            source_profile: String::new(),
            benchmark_eligible: false,
            leaves: Vec::new(),
        };
        assert_eq!(
            state.begin_snapshot(&snapshot),
            SnapshotDisposition::ConfirmEpoch
        );
        assert_eq!(
            state.begin_snapshot(&snapshot),
            SnapshotDisposition::EpochReset
        );
        assert!(state.value(&path).is_none());
    }

    #[test]
    fn seek_seconds_clamp_to_the_transport_domain() {
        assert_eq!(clamp_seek_seconds(12.5, 331.0), 12.5);
        assert_eq!(clamp_seek_seconds(-3.0, 331.0), 0.0);
        assert_eq!(clamp_seek_seconds(400.0, 331.0), 331.0);
        // Unbounded (multitone) length: no upper clamp.
        assert_eq!(clamp_seek_seconds(400.0, 0.0), 400.0);
        assert_eq!(clamp_seek_seconds(-1.0, 0.0), 0.0);
    }

    #[test]
    fn transient_position_revision_does_not_confirm_an_epoch() {
        // The position changed feed stamps the GLOBAL store revision; a
        // snapshot reports the leaf's own (seek-target) revision. That
        // disagreement is routine after any write and must never read as a
        // restarted authority (it reset the extrapolation clock every
        // snapshot refresh — the waves-playhead flicker).
        let mut state = MusicdMixerState {
            ready: true,
            snapshot_revision: Some(1),
            ..default()
        };
        assert!(state.accept_leaf(
            TRANSPORT_POSITION_PATH.to_string(),
            LeafValue::Number(1.9),
            1
        ));
        let snapshot = MixerSnapshotResponse {
            revision: 1,
            real_audio: true,
            audio_fault: false,
            applied_fault: false,
            source_profile: String::new(),
            benchmark_eligible: false,
            leaves: vec![cosmix_mixer_schema::LeafSnapshot {
                path: TRANSPORT_POSITION_PATH.to_string(),
                value: LeafValue::Number(0.0),
                revision: 0,
            }],
        };
        assert_eq!(
            state.begin_snapshot(&snapshot),
            SnapshotDisposition::Applied
        );
        assert_eq!(
            state.begin_snapshot(&snapshot),
            SnapshotDisposition::Applied
        );
    }

    #[test]
    fn per_path_rollback_detects_restart_after_global_revision_catches_up() {
        let mut state = MusicdMixerState {
            ready: true,
            snapshot_revision: Some(5),
            ..default()
        };
        let path = "mixer.channels.0.fader".to_string();
        assert!(state.accept_leaf(path.clone(), LeafValue::Number(-6.0), 40));
        let snapshot = MixerSnapshotResponse {
            revision: 40,
            real_audio: false,
            audio_fault: false,
            applied_fault: false,
            source_profile: String::new(),
            benchmark_eligible: false,
            leaves: vec![cosmix_mixer_schema::LeafSnapshot {
                path: path.clone(),
                value: LeafValue::Number(0.0),
                revision: 0,
            }],
        };

        assert_eq!(
            state.begin_snapshot(&snapshot),
            SnapshotDisposition::ConfirmEpoch
        );
        assert_eq!(
            state.begin_snapshot(&snapshot),
            SnapshotDisposition::EpochReset
        );
        assert!(state.value(&path).is_none());
    }

    #[test]
    fn mixer_mapping_keeps_unity_at_three_quarters_travel() {
        let mapping = default_fader_mapping();
        assert!((mapping.to_position(0.0) - 0.75).abs() < 1e-6);
        assert_eq!(mapping.to_value(0.0), FADER_MIN_DB as f32);
        assert_eq!(mapping.to_value(1.0), FADER_MAX_DB as f32);
    }

    #[test]
    fn boolean_binding_polarity_is_true_when_active() {
        let binding = MixerBinding::boolean("mixer.channels.0.mute");
        assert_eq!(binding.leaf_value(1.0), LeafValue::Bool(true));
        assert_eq!(binding.leaf_value(0.0), LeafValue::Bool(false));
    }

    #[test]
    fn enum_write_binding_commits_its_fixed_string_regardless_of_value() {
        let binding = MixerBinding::enum_write("transport.state", "playing");
        // The momentary button's 0.0 (or any) value is ignored — the enum wins.
        assert_eq!(binding.leaf_value(0.0), LeafValue::Enum("playing".into()));
        assert_eq!(binding.leaf_value(1.0), LeafValue::Enum("playing".into()));
        // A plain number binding is unaffected.
        assert_eq!(
            MixerBinding::number("transport.position").leaf_value(0.0),
            LeafValue::Number(0.0)
        );
    }

    #[test]
    fn strip_style_compact_selects_the_compact_layout() {
        assert!(!StripStyle::default().is_compact());
        assert!(StripStyle::compact().is_compact());
        // The board strip is far narrower than the default and than its cutoff.
        assert!(StripStyle::compact().width < StripStyle::default().width);
        assert!(StripStyle::compact().width <= COMPACT_STRIP_MAX_WIDTH);
        assert!(StripStyle::default().width > COMPACT_STRIP_MAX_WIDTH);
    }

    #[test]
    fn position_extrapolation_advances_only_while_playing_and_clamps_to_length() {
        // Stopped: the position holds at the base regardless of elapsed time.
        assert_eq!(extrapolate_position_seconds(10.0, 5.0, false, 200.0), 10.0);
        // Playing: advance by wall-clock.
        assert_eq!(extrapolate_position_seconds(10.0, 5.0, true, 200.0), 15.0);
        // Clamped to a known length (never past the end).
        assert_eq!(extrapolate_position_seconds(198.0, 5.0, true, 200.0), 200.0);
        // Unbounded (length 0): no upper clamp, matching the daemon.
        assert_eq!(
            extrapolate_position_seconds(10_000.0, 5.0, true, 0.0),
            10_005.0
        );
        // A negative base can never drive the thumb below zero.
        assert_eq!(extrapolate_position_seconds(-3.0, 0.0, false, 0.0), 0.0);
    }

    #[test]
    fn mmss_formats_minutes_and_zero_padded_seconds() {
        assert_eq!(format_mmss(0.0), "0:00");
        assert_eq!(format_mmss(9.4), "0:09");
        assert_eq!(format_mmss(65.0), "1:05");
        assert_eq!(format_mmss(3599.0), "59:59");
        // Non-finite / negative inputs never panic; they read as 0:00.
        assert_eq!(format_mmss(-1.0), "0:00");
        assert_eq!(format_mmss(f64::NAN), "0:00");
    }

    #[test]
    fn scrubber_range_falls_back_when_length_is_unbounded() {
        let (bounded, _) = scrubber_range_for(180.0);
        assert_eq!(bounded.min, 0.0);
        assert_eq!(bounded.max, 180.0);
        let (unbounded, _) = scrubber_range_for(0.0);
        assert_eq!(unbounded.max, SCRUBBER_FALLBACK_LENGTH_SECS);
    }

    #[test]
    fn snapshot_change_buffer_keeps_latest_value_per_path() {
        let mut io = MixerIo::default();
        let path = "mixer.channels.0.fader".to_string();
        buffer_change(
            &mut io,
            ChangedEvent {
                path: path.clone(),
                revision: 3,
                value: LeafValue::Number(-12.0),
                source_id: None,
            },
        );
        buffer_change(
            &mut io,
            ChangedEvent {
                path: path.clone(),
                revision: 4,
                value: LeafValue::Number(-6.0),
                source_id: None,
            },
        );
        buffer_change(
            &mut io,
            ChangedEvent {
                path: path.clone(),
                revision: 2,
                value: LeafValue::Number(-30.0),
                source_id: None,
            },
        );

        let buffered = &io.buffered_changes.get(&path).unwrap().event;
        assert_eq!(buffered.revision, 4);
        assert_eq!(buffered.value, LeafValue::Number(-6.0));
    }

    #[test]
    fn snapshot_replay_uses_only_the_current_connection_generation() {
        let mut io = MixerIo {
            sync_generation: 1,
            ..default()
        };
        buffer_change(
            &mut io,
            ChangedEvent {
                path: "mixer.channels.0.fader".into(),
                revision: 50,
                value: LeafValue::Number(-30.0),
                source_id: None,
            },
        );
        io.sync_generation = 2;
        buffer_change(
            &mut io,
            ChangedEvent {
                path: "mixer.channels.0.pan".into(),
                revision: 4,
                value: LeafValue::Number(0.25),
                source_id: None,
            },
        );

        let replay = take_replayable_changes(&mut io, 3);
        assert_eq!(replay.len(), 1);
        assert_eq!(replay[0].path, "mixer.channels.0.pan");
        assert_eq!(replay[0].revision, 4);
    }

    #[test]
    fn snapshot_replays_equal_revision_transient_updates() {
        let mut io = MixerIo {
            sync_generation: 1,
            ..default()
        };
        buffer_change(
            &mut io,
            ChangedEvent {
                path: "transport.position".into(),
                revision: 3,
                value: LeafValue::Number(12.5),
                source_id: None,
            },
        );

        let replay = take_replayable_changes(&mut io, 3);
        assert_eq!(replay.len(), 1);
        assert_eq!(replay[0].path, "transport.position");
        assert_eq!(replay[0].value, LeafValue::Number(12.5));
        assert!(!snapshot_covers_change(Some(3), 3));
        assert!(snapshot_covers_change(Some(3), 2));
    }

    #[test]
    fn queued_messages_from_an_old_connection_are_rejected() {
        let io = MixerIo {
            sync_generation: 3,
            ..default()
        };
        let message = |generation: u64| TransportMessage::Applied {
            generation,
            applied: cosmix_mixer_schema::DspApplied {
                revision: 1,
                sample_frame: 0,
            },
        };

        // The pump's generation filter: only current-epoch messages pass.
        assert_ne!(message(2).generation(), io.sync_generation);
        assert_eq!(message(3).generation(), io.sync_generation);
    }

    #[test]
    fn inflight_local_edit_owns_the_view_until_its_reply() {
        let path = "mixer.channels.0.fader";
        let mut io = MixerIo::default();
        assert!(should_update_view(&io, path));
        io.inflight_paths.insert(path.into());
        assert!(!should_update_view(&io, path));
        io.inflight_paths.clear();
        io.active_gestures
            .insert(path.into(), test_active_gesture(1));
        assert!(!should_update_view(&io, path));
        assert!(!write_reply_updates_view(&io, path));
        io.active_gestures.clear();
        assert!(write_reply_updates_view(&io, path));
    }

    #[test]
    fn stream_gate_respects_min_interval() {
        let path = "mixer.channels.0.fader";
        let mut io = MixerIo::default();
        // Never-streamed path is immediately due.
        assert!(stream_due(&io, path));
        // Just-streamed path is gated.
        io.last_stream_issue.insert(path.into(), Instant::now());
        assert!(!stream_due(&io, path));
        // A path whose spacing has elapsed is due again.
        let elapsed = Instant::now()
            .checked_sub(STREAM_MIN_INTERVAL + Duration::from_millis(4))
            .expect("clock supports backdating");
        io.last_stream_issue.insert(path.into(), elapsed);
        assert!(stream_due(&io, path));
    }

    #[test]
    fn write_ack_never_drains_the_outbox_directly() {
        let path = "mixer.channels.0.fader";
        let mut io = MixerIo::default();
        io.inflight_paths.insert(path.into());
        io.queued_latest
            .insert(path.into(), queued_number(-6.0, 1, None));
        finish_write(path, &mut io);
        // The in-flight claim is released; the queued value stays for the
        // durable outbox (one issuer, one failure path).
        assert!(!io.inflight_paths.contains(path));
        assert!(io.queued_latest.contains_key(path));
    }

    #[test]
    fn latest_wins_supersession_preserves_new_envelope() {
        let path = "transport.position";
        let mut io = MixerIo::default();
        let first = QueuedWrite::new(LeafValue::Number(10.0), test_meta(10, Some(1), "source-a"));
        let second = QueuedWrite::new(LeafValue::Number(20.0), test_meta(11, Some(2), "source-b"));

        io.queue_write(path, first);
        io.queue_write(path, second);

        let queued = io.queued_latest.get(path).unwrap();
        assert_eq!(queued.value, LeafValue::Number(20.0));
        assert_eq!(queued.meta.source, CommandSource::app("source-b"));
        assert_eq!(queued.meta.gesture_id, Some(GestureId(2)));
        assert_eq!(queued.meta.generation, CommandGeneration(11));
        assert_eq!(io.command_outcomes.len(), 1);
        assert_eq!(
            io.command_outcomes[0],
            TransportCommandOutcome {
                path: path.into(),
                source: CommandSource::app("source-a"),
                gesture_id: Some(GestureId(1)),
                generation: CommandGeneration(10),
                terminal: CommandTerminal::Superseded {
                    by: CommandGeneration(11),
                },
            }
        );

        // Same-gesture stream churn is internally latest-wins but intentionally
        // silent on the public outcome bus.
        io.queue_write(path, queued_number(30.0, 12, Some(9)));
        io.command_outcomes.clear();
        io.queue_write(path, queued_number(40.0, 13, Some(9)));
        assert!(io.command_outcomes.is_empty());
    }

    #[test]
    fn envelope_survives_queue_issue_ack_and_applied_coverage() {
        let path = "mixer.channels.0.fader";
        let source = CommandSource::app("studio:key");
        let gesture_id = GestureId(41);
        let generation = CommandGeneration(42);
        let transport_state = Arc::new(Mutex::new(RecordingTransportState::default()));
        let transport = RecordingTransport {
            state: Arc::clone(&transport_state),
        };
        let mut mixer_state = MusicdMixerState {
            connection: MixerConnectionState::Connected,
            ready: true,
            ..default()
        };
        mixer_state.revisions.insert(path.into(), 7);
        mixer_state
            .values
            .insert(path.into(), LeafValue::Number(-12.0));
        let mut io = MixerIo::default();
        io.active_gestures.insert(
            path.into(),
            ActiveGesture {
                id: gesture_id,
                owner: Entity::PLACEHOLDER,
            },
        );
        io.queue_write(
            path,
            QueuedWrite::new(
                LeafValue::Number(-6.0),
                CommandMeta {
                    source: source.clone(),
                    gesture_id: Some(gesture_id),
                    generation,
                },
            ),
        );

        let mut app = App::new();
        app.insert_resource(MixerTransportRes(Box::new(transport)))
            .insert_resource(mixer_state)
            .insert_resource(io)
            .init_resource::<LatestMeter>()
            .init_resource::<TransportPosition>()
            .add_systems(PreUpdate, pump_transport)
            .add_systems(Update, flush_queued_writes);

        app.update();
        let (request_id, request) = transport_state.lock().unwrap().writes[0].clone();
        assert_eq!(request.path, path);
        assert_eq!(request.value, LeafValue::Number(-6.0));
        assert_eq!(request.if_revision, Some(7), "CAS floor must be preserved");

        {
            let mut recorded = transport_state.lock().unwrap();
            recorded.events.push(TransportEvent::Reply {
                request_id,
                result: Ok(TransportReply::Write(Ok(WriteResponse::Accepted(
                    cosmix_mixer_schema::WriteAck {
                        revision: 8,
                        path: path.into(),
                        canonical_value: LeafValue::Number(-6.0),
                        source_id: "ctk-test".into(),
                        op_id: request.op_id,
                    },
                )))),
                completed_at: Some(Instant::now()),
            });
            recorded.messages.push(TransportMessage::Applied {
                generation: 0,
                applied: cosmix_mixer_schema::DspApplied {
                    revision: 8,
                    sample_frame: 512,
                },
            });
        }
        app.update();

        let io = app.world().resource::<MixerIo>();
        assert_eq!(io.command_outcomes.len(), 1);
        assert_eq!(
            io.command_outcomes[0],
            TransportCommandOutcome {
                path: path.into(),
                source,
                gesture_id: Some(gesture_id),
                generation,
                terminal: CommandTerminal::CoveredByAppliedRevision {
                    accepted_revision: 8,
                    applied_revision: 8,
                },
            }
        );
        assert!(io.awaiting_applied.is_empty());
    }

    #[test]
    fn lifecycle_reset_abandons_every_live_command_exactly_once() {
        let mut io = MixerIo::default();
        io.queued_latest.insert(
            "queued".into(),
            QueuedWrite::new(LeafValue::Number(1.0), test_meta(10, None, "queued")),
        );
        io.pending.insert(
            1,
            RequestKind::Write {
                write: issued_number("pending", 2.0, Some(1), 11, None),
                attempt: 0,
            },
        );
        io.retries.push(RetryWrite {
            due: Instant::now(),
            write: issued_number("retry", 3.0, Some(2), 12, None),
            attempt: 1,
        });
        io.awaiting_applied.push(AwaitingCoverage {
            path: "acknowledged".into(),
            accepted_revision: 44,
            issued: Some(Instant::now()),
            meta: test_meta(13, None, "acknowledged"),
        });

        reset_epoch_io(&mut io);

        assert!(io.queued_latest.is_empty());
        assert!(io.pending.is_empty());
        assert!(io.retries.is_empty());
        assert!(io.awaiting_applied.is_empty());
        assert_eq!(io.command_outcomes.len(), 4);

        let phase = |generation| {
            io.command_outcomes
                .iter()
                .find(|outcome| outcome.generation == CommandGeneration(generation))
                .map(|outcome| &outcome.terminal)
                .expect("every seeded generation has one outcome")
        };
        assert_eq!(
            phase(10),
            &CommandTerminal::Abandoned {
                at: LastKnownPhase::Desired,
                reason: LifecycleReset::AuthorityEpochChanged,
            }
        );
        assert_eq!(
            phase(11),
            &CommandTerminal::Abandoned {
                at: LastKnownPhase::Issued,
                reason: LifecycleReset::AuthorityEpochChanged,
            }
        );
        assert_eq!(phase(12), phase(11));
        assert_eq!(
            phase(13),
            &CommandTerminal::Abandoned {
                at: LastKnownPhase::Acknowledged { revision: 44 },
                reason: LifecycleReset::AuthorityEpochChanged,
            }
        );
        for generation in 10..=13 {
            assert_eq!(
                io.command_outcomes
                    .iter()
                    .filter(|outcome| outcome.generation == CommandGeneration(generation))
                    .count(),
                1,
                "generation {generation} must have exactly one terminal outcome"
            );
        }
    }

    #[test]
    fn competing_controls_do_not_share_or_tear_down_a_path_gesture() {
        let path = "mixer.channels.0.fader";
        let transport_state = Arc::new(Mutex::new(RecordingTransportState::default()));
        let transport = RecordingTransport {
            state: Arc::clone(&transport_state),
        };
        let mut state = MusicdMixerState {
            connection: MixerConnectionState::Connected,
            ready: true,
            ..default()
        };
        state.values.insert(path.into(), LeafValue::Number(-12.0));
        state.revisions.insert(path.into(), 7);

        let mut app = App::new();
        app.insert_resource(MixerTransportRes(Box::new(transport)))
            .insert_resource(state)
            .init_resource::<MixerIo>()
            .init_resource::<TransportPosition>()
            .add_observer(on_control_change);
        let control_a = app.world_mut().spawn(MixerBinding::number(path)).id();
        let control_b = app.world_mut().spawn(MixerBinding::number(path)).id();

        app.world_mut().trigger(ControlChange {
            source: control_a,
            value: -6.0,
            is_final: false,
        });
        let gesture_a = {
            let io = app.world().resource::<MixerIo>();
            let active = io.active_gestures.get(path).copied().unwrap();
            assert_eq!(active.owner, control_a);
            assert_eq!(
                io.queued_latest.get(path).unwrap().meta.gesture_id,
                Some(active.id)
            );
            active.id
        };

        app.world_mut().trigger(ControlChange {
            source: control_b,
            value: -3.0,
            is_final: false,
        });
        let gesture_b = {
            let io = app.world().resource::<MixerIo>();
            let active = io.active_gestures.get(path).copied().unwrap();
            assert_eq!(active.owner, control_a, "B must not steal path ownership");
            assert_eq!(active.id, gesture_a);
            assert!(io.gesture_baseline.contains_key(path));

            let issued_b = io
                .pending
                .values()
                .find_map(|kind| match kind {
                    RequestKind::Write { write, .. } if write.request.path == path => Some(write),
                    _ => None,
                })
                .expect("B's independent command is issued");
            assert_eq!(issued_b.meta.source, CommandSource::control(control_b));
            let gesture_b = issued_b.meta.gesture_id.expect("B has its own lineage");
            assert_ne!(gesture_b, gesture_a);
            assert_eq!(gesture_b.0, issued_b.meta.generation.0);

            assert_eq!(io.command_outcomes.len(), 1);
            assert_eq!(
                io.command_outcomes[0].source,
                CommandSource::control(control_a)
            );
            assert_eq!(io.command_outcomes[0].gesture_id, Some(gesture_a));
            assert_eq!(
                io.command_outcomes[0].terminal,
                CommandTerminal::Superseded {
                    by: issued_b.meta.generation,
                },
                "cross-owner supersession must not be flood-suppressed"
            );
            gesture_b
        };

        app.world_mut().trigger(ControlChange {
            source: control_a,
            value: 0.0,
            is_final: true,
        });
        let io = app.world().resource::<MixerIo>();
        assert!(!io.active_gestures.contains_key(path));
        assert!(!io.gesture_baseline.contains_key(path));
        let final_a = io
            .queued_latest
            .get(path)
            .expect("A's final queues behind B's in-flight command");
        assert_eq!(final_a.meta.source, CommandSource::control(control_a));
        assert_eq!(final_a.meta.gesture_id, Some(gesture_a));
        assert_ne!(final_a.meta.gesture_id, Some(gesture_b));
    }

    #[test]
    fn ruler_gesture_owns_position_and_rejects_competing_footer_scrub() {
        let (mut app, _transport) = transport_gesture_test_app();
        let ruler = app.world_mut().spawn_empty().id();
        let footer = app
            .world_mut()
            .spawn(MixerBinding::number(TRANSPORT_POSITION_PATH))
            .id();

        app.world_mut().trigger(TransportSeekGesture {
            source: ruler,
            phase: TransportSeekGesturePhase::Begin { seconds: 20.0 },
        });
        let gesture_id = {
            let io = app.world().resource::<MixerIo>();
            let active = io.active_gestures[TRANSPORT_POSITION_PATH];
            assert_eq!(active.owner, ruler);
            active.id
        };

        app.world_mut().trigger(ControlChange {
            source: footer,
            value: 30.0,
            is_final: false,
        });
        {
            let io = app.world().resource::<MixerIo>();
            assert_eq!(io.active_gestures[TRANSPORT_POSITION_PATH].owner, ruler);
            assert_eq!(io.command_outcomes.len(), 1);
            assert_eq!(
                io.command_outcomes[0].source,
                CommandSource::control(footer)
            );
            assert!(matches!(
                io.command_outcomes[0].terminal,
                CommandTerminal::Rejected { .. }
            ));
        }

        app.world_mut().trigger(TransportSeekGesture {
            source: ruler,
            phase: TransportSeekGesturePhase::Update { seconds: 40.0 },
        });
        let io = app.world().resource::<MixerIo>();
        assert_eq!(io.active_gestures[TRANSPORT_POSITION_PATH].owner, ruler);
        let queued = &io.queued_latest[TRANSPORT_POSITION_PATH];
        assert_eq!(queued.value, LeafValue::Number(40.0));
        assert_eq!(queued.meta.gesture_id, Some(gesture_id));
        assert_eq!(queued.meta.source.entity, Some(ruler));
        assert_eq!(
            io.command_outcomes
                .iter()
                .filter(|outcome| matches!(outcome.terminal, CommandTerminal::Rejected { .. }))
                .count(),
            1,
            "the owner's update must continue after rejecting the footer"
        );
    }

    #[test]
    fn footer_scrub_owns_position_and_rejects_ruler_begin() {
        let (mut app, transport) = transport_gesture_test_app();
        let ruler = app.world_mut().spawn_empty().id();
        let footer = app
            .world_mut()
            .spawn(MixerBinding::number(TRANSPORT_POSITION_PATH))
            .id();

        app.world_mut().trigger(ControlChange {
            source: footer,
            value: 20.0,
            is_final: false,
        });
        let footer_gesture =
            app.world().resource::<MixerIo>().active_gestures[TRANSPORT_POSITION_PATH];
        assert_eq!(footer_gesture.owner, footer);

        app.world_mut().trigger(TransportSeekGesture {
            source: ruler,
            phase: TransportSeekGesturePhase::Begin { seconds: 30.0 },
        });
        {
            let io = app.world().resource::<MixerIo>();
            assert_eq!(io.active_gestures[TRANSPORT_POSITION_PATH], footer_gesture);
            assert_eq!(
                io.queued_latest[TRANSPORT_POSITION_PATH].meta.source,
                CommandSource::control(footer)
            );
            assert_eq!(io.command_outcomes.len(), 1);
            assert_eq!(io.command_outcomes[0].source.entity, Some(ruler));
            assert!(matches!(
                io.command_outcomes[0].terminal,
                CommandTerminal::Rejected { .. }
            ));
        }

        // The tightened position guard must not disturb the owning footer's
        // ordinary stream or final commit.
        app.world_mut().trigger(ControlChange {
            source: footer,
            value: 40.0,
            is_final: false,
        });
        app.world_mut().trigger(ControlChange {
            source: footer,
            value: 50.0,
            is_final: true,
        });
        let io = app.world().resource::<MixerIo>();
        assert!(!io.active_gestures.contains_key(TRANSPORT_POSITION_PATH));
        let issued = io
            .pending
            .values()
            .find_map(|kind| match kind {
                RequestKind::Write { write, .. }
                    if write.request.path == TRANSPORT_POSITION_PATH =>
                {
                    Some(write)
                }
                _ => None,
            })
            .expect("the footer owner's final commit is issued");
        assert_eq!(issued.meta.source, CommandSource::control(footer));
        assert_eq!(issued.meta.gesture_id, Some(footer_gesture.id));
        assert_eq!(issued.request.value, LeafValue::Number(50.0));
        assert_eq!(transport.lock().unwrap().writes.len(), 1);
    }

    #[test]
    fn ruler_cancel_purges_stream_and_writes_live_position_baseline() {
        let (mut app, transport) = transport_gesture_test_app();
        let ruler = app.world_mut().spawn_empty().id();

        app.world_mut().trigger(TransportSeekGesture {
            source: ruler,
            phase: TransportSeekGesturePhase::Begin { seconds: 20.0 },
        });
        app.world_mut().trigger(TransportSeekGesture {
            source: ruler,
            phase: TransportSeekGesturePhase::Update { seconds: 30.0 },
        });
        let gesture_id = {
            let mut io = app.world_mut().resource_mut::<MixerIo>();
            let active = io.active_gestures[TRANSPORT_POSITION_PATH];
            let retry_generation = io.command_generation();
            io.retries.push(RetryWrite {
                due: Instant::now(),
                write: IssuedWrite {
                    request: WriteRequest {
                        path: TRANSPORT_POSITION_PATH.into(),
                        value: LeafValue::Number(25.0),
                        op_id: "test-ruler-stale-stream".into(),
                        if_revision: Some(7),
                    },
                    meta: CommandMeta {
                        source: CommandSource::app_entity("ctk:transport-seek-gesture", ruler),
                        gesture_id: Some(active.id),
                        generation: retry_generation,
                    },
                },
                attempt: 1,
            });
            io.inflight_paths.insert(TRANSPORT_POSITION_PATH.into());
            io.own_write_revisions
                .entry(TRANSPORT_POSITION_PATH.into())
                .or_default()
                .insert(8);
            active.id
        };

        app.world_mut().trigger(TransportSeekGesture {
            source: ruler,
            phase: TransportSeekGesturePhase::Cancel,
        });

        let io = app.world().resource::<MixerIo>();
        assert!(!io.active_gestures.contains_key(TRANSPORT_POSITION_PATH));
        assert!(!io.queued_latest.contains_key(TRANSPORT_POSITION_PATH));
        assert!(!io.gesture_baseline.contains_key(TRANSPORT_POSITION_PATH));
        assert!(!io.own_write_revisions.contains_key(TRANSPORT_POSITION_PATH));
        assert!(io.retries.is_empty());
        let compensation = io
            .pending
            .values()
            .find_map(|kind| match kind {
                RequestKind::Write { write, .. }
                    if write.request.path == TRANSPORT_POSITION_PATH =>
                {
                    Some(write)
                }
                _ => None,
            })
            .expect("cancel issues the baseline compensation");
        assert_eq!(compensation.request.value, LeafValue::Number(12.0));
        assert_eq!(compensation.meta.gesture_id, Some(gesture_id));
        assert_eq!(compensation.meta.source.entity, Some(ruler));
        assert!(io.command_outcomes.is_empty());

        let writes = &transport.lock().unwrap().writes;
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].1.value, LeafValue::Number(12.0));
        assert!(writes
            .iter()
            .all(|(_, request)| request.value != LeafValue::Number(30.0)));
    }

    #[test]
    fn ruler_stream_updates_do_not_emit_superseded_outcomes() {
        let (mut app, _transport) = transport_gesture_test_app();
        let ruler = app.world_mut().spawn_empty().id();

        app.world_mut().trigger(TransportSeekGesture {
            source: ruler,
            phase: TransportSeekGesturePhase::Begin { seconds: 10.0 },
        });
        let gesture_id =
            app.world().resource::<MixerIo>().active_gestures[TRANSPORT_POSITION_PATH].id;
        for seconds in [20.0, 30.0, 40.0, 50.0] {
            app.world_mut().trigger(TransportSeekGesture {
                source: ruler,
                phase: TransportSeekGesturePhase::Update { seconds },
            });
        }

        let io = app.world().resource::<MixerIo>();
        assert_eq!(
            io.queued_latest[TRANSPORT_POSITION_PATH].value,
            LeafValue::Number(50.0)
        );
        assert_eq!(
            io.queued_latest[TRANSPORT_POSITION_PATH].meta.gesture_id,
            Some(gesture_id)
        );
        assert_eq!(
            io.command_outcomes
                .iter()
                .filter(|outcome| matches!(outcome.terminal, CommandTerminal::Superseded { .. }))
                .count(),
            0
        );
    }

    #[test]
    fn persistent_local_issue_failure_rejects_once_and_empties_outbox() {
        let path = "mixer.channels.0.fader";
        let transport_state = Arc::new(Mutex::new(RecordingTransportState {
            local_issue_error: Some("local queue unavailable".into()),
            ..default()
        }));
        let transport = RecordingTransport {
            state: Arc::clone(&transport_state),
        };
        let state = MusicdMixerState {
            connection: MixerConnectionState::Connected,
            ready: true,
            ..default()
        };
        let mut app = App::new();
        app.insert_resource(MixerTransportRes(Box::new(transport)))
            .insert_resource(state)
            .init_resource::<MixerIo>()
            .init_resource::<TransportPosition>()
            .add_observer(on_control_change)
            .add_systems(Update, flush_queued_writes);
        let control = app.world_mut().spawn(MixerBinding::number(path)).id();

        app.world_mut().trigger(ControlChange {
            source: control,
            value: -6.0,
            is_final: true,
        });
        let (generation, entered) = {
            let io = app.world().resource::<MixerIo>();
            let queued = io.queued_latest.get(path).expect("first failure requeues");
            assert_eq!(queued.local_issue_attempts, 1);
            assert_eq!(queued.meta.source, CommandSource::control(control));
            (
                queued.meta.generation,
                *io.queued_since.get(path).expect("queue stamp retained"),
            )
        };

        // Pin the backoff deadline far into the future so "still inside the
        // window" is deterministic — otherwise a slow frame (>8ms, the first
        // backoff step) lets the flush reissue and races wall-clock.
        {
            let mut io = app.world_mut().resource_mut::<MixerIo>();
            io.queued_latest.get_mut(path).unwrap().local_issue_due =
                Some(Instant::now() + Duration::from_secs(3600));
        }
        // An ordinary next frame is still inside the local backoff window.
        app.update();
        assert_eq!(transport_state.lock().unwrap().writes.len(), 1);
        assert_eq!(
            app.world()
                .resource::<MixerIo>()
                .queued_latest
                .get(path)
                .unwrap()
                .local_issue_attempts,
            1
        );

        for expected_attempt in 2..=LOCAL_ISSUE_MAX_ATTEMPTS {
            {
                let mut io = app.world_mut().resource_mut::<MixerIo>();
                let queued = io.queued_latest.get_mut(path).unwrap();
                queued.local_issue_due = Instant::now().checked_sub(Duration::from_millis(1));
            }
            app.update();
            let io = app.world().resource::<MixerIo>();
            if expected_attempt < LOCAL_ISSUE_MAX_ATTEMPTS {
                assert_eq!(
                    io.queued_latest.get(path).unwrap().local_issue_attempts,
                    expected_attempt
                );
                assert_eq!(io.queued_since.get(path), Some(&entered));
            }
        }

        let io = app.world().resource::<MixerIo>();
        assert!(io.queued_latest.is_empty());
        assert!(!io.queued_since.contains_key(path));
        assert!(
            io.pending.is_empty(),
            "local failures never become wire-pending"
        );
        assert_eq!(io.command_outcomes.len(), 1);
        assert_eq!(
            io.command_outcomes[0],
            TransportCommandOutcome {
                path: path.into(),
                source: CommandSource::control(control),
                gesture_id: None,
                generation,
                terminal: CommandTerminal::Rejected {
                    reason: format!(
                        "local transport issue failed after {LOCAL_ISSUE_MAX_ATTEMPTS} attempts: local queue unavailable"
                    ),
                },
            }
        );

        let recorded = transport_state.lock().unwrap();
        assert_eq!(recorded.writes.len(), usize::from(LOCAL_ISSUE_MAX_ATTEMPTS));
        let op_ids: HashSet<_> = recorded
            .writes
            .iter()
            .map(|(_, request)| request.op_id.as_str())
            .collect();
        assert_eq!(
            op_ids.len(),
            recorded.writes.len(),
            "every unsent retry must mint a fresh op_id"
        );
    }

    #[test]
    fn cancelled_gesture_releases_view_ownership_and_restores_authority() {
        use crate::widgets::CtkWidgetsPlugin;

        let path = "mixer.channels.0.pan";
        let mut app = App::new();
        app.add_plugins(CtkWidgetsPlugin)
            .init_resource::<MusicdMixerState>()
            .init_resource::<MixerIo>()
            .add_observer(on_control_gesture_cancel);
        let control = app
            .world_mut()
            .spawn((
                knob(NumericControlProps::new(
                    "test.cancelled-pan",
                    0.75,
                    ControlRange {
                        min: -1.0,
                        max: 1.0,
                        step: 0.1,
                        detent: Some(0.0),
                    },
                    ValueMapping::linear(-1.0, 1.0).unwrap(),
                )),
                MixerBinding::number(path),
            ))
            .id();
        app.world_mut()
            .resource_mut::<MusicdMixerState>()
            .values
            .insert(path.into(), LeafValue::Number(-0.25));
        {
            let mut io = app.world_mut().resource_mut::<MixerIo>();
            io.active_gestures.insert(
                path.into(),
                ActiveGesture {
                    id: GestureId(1),
                    owner: control,
                },
            );
            io.queued_latest.insert(
                path.into(),
                QueuedWrite::new(
                    LeafValue::Number(0.9),
                    CommandMeta {
                        source: CommandSource::control(control),
                        gesture_id: Some(GestureId(1)),
                        generation: CommandGeneration(2),
                    },
                ),
            );
        }

        app.world_mut()
            .trigger(ControlGestureCancel { source: control });
        app.update();

        assert!(!app
            .world()
            .resource::<MixerIo>()
            .active_gestures
            .contains_key(path));
        // The abandoned streaming intent must not survive to a later drain.
        assert!(!app
            .world()
            .resource::<MixerIo>()
            .queued_latest
            .contains_key(path));
        assert_eq!(
            app.world().get::<ControlValue>(control),
            Some(&ControlValue(-0.25))
        );
    }

    #[test]
    fn name_lines_split_on_words_and_pack_short_words() {
        assert_eq!(split_name_lines("Night Run"), "Night\nRun");
        assert_eq!(split_name_lines("Ch 1"), "Ch 1");
        assert_eq!(split_name_lines("Lead Vox"), "Lead\nVox");
        // A word longer than the cap is hard-trimmed — an overflowing line
        // would bleed across the neighbouring strip.
        assert_eq!(split_name_lines("Overheads L"), "Overhe\nL");
        assert_eq!(split_name_lines("Synth Strings"), "Synth\nString");
        assert_eq!(split_name_lines("Slap Bass 2"), "Slap\nBass 2");
        assert_eq!(split_name_lines(""), "");
    }

    #[test]
    fn scrub_baseline_seeds_from_the_live_clock_not_the_seek_target() {
        let io = MixerIo::default();
        let mut state = MusicdMixerState::default();
        // Stored leaf = last seek target (0), song actually at 65.2s.
        state
            .values
            .insert(TRANSPORT_POSITION_PATH.into(), LeafValue::Number(0.0));
        state.revisions.insert(TRANSPORT_POSITION_PATH.into(), 7);

        let seeded =
            seed_gesture_baseline(&io, &state, TRANSPORT_POSITION_PATH, Some(65.2)).unwrap();
        assert_eq!(seeded, (LeafValue::Number(65.2), Some(7)));

        // No clock yet (pre-first-poll): fall back to the stored leaf.
        let fallback = seed_gesture_baseline(&io, &state, TRANSPORT_POSITION_PATH, None).unwrap();
        assert_eq!(fallback, (LeafValue::Number(0.0), Some(7)));

        // Ordinary paths ignore the live position entirely.
        state
            .values
            .insert("mixer.channels.0.fader".into(), LeafValue::Number(-6.0));
        state.revisions.insert("mixer.channels.0.fader".into(), 9);
        let ordinary =
            seed_gesture_baseline(&io, &state, "mixer.channels.0.fader", Some(65.2)).unwrap();
        assert_eq!(ordinary, (LeafValue::Number(-6.0), Some(9)));
    }

    #[test]
    fn transport_position_leaf_never_drives_the_scrubber_view() {
        use crate::widgets::CtkWidgetsPlugin;
        use bevy::ecs::system::SystemState;

        let mut app = App::new();
        app.add_plugins(CtkWidgetsPlugin);
        let (range, mapping) = scrubber_range_for(300.0);
        let scrubber = app
            .world_mut()
            .spawn((
                hfader_sized(
                    NumericControlProps::new("transport-position", 42.0, range, mapping),
                    300.0,
                    24.0,
                ),
                MixerBinding::number(TRANSPORT_POSITION_PATH),
                TransportScrubber,
            ))
            .id();
        let fader = app
            .world_mut()
            .spawn((
                fader_sized(
                    NumericControlProps::new(
                        "test-fader",
                        0.0,
                        ControlRange {
                            min: FADER_MIN_DB as f32,
                            max: FADER_MAX_DB as f32,
                            step: 0.1,
                            detent: None,
                        },
                        default_fader_mapping(),
                    ),
                    12.0,
                    300.0,
                ),
                MixerBinding::number("mixer.channels.0.fader"),
            ))
            .id();
        app.update();

        type ApplyLeafParams<'w, 's> = (
            Query<'w, 's, (Entity, &'static MixerBinding)>,
            Query<'w, 's, (&'static MixerName, &'static mut Text)>,
            Commands<'w, 's>,
        );
        let mut mixer_state = MusicdMixerState::default();
        let mut params: SystemState<ApplyLeafParams> = SystemState::new(app.world_mut());
        let (bindings, mut names, mut commands) = params
            .get_mut(app.world_mut())
            .expect("test system params are available");
        // The stored transport.position leaf is a SEEK TARGET, not the live
        // clock — even with update_view it must never move the scrubber
        // (the flick-to-zero-on-snapshot-refresh regression).
        assert!(apply_leaf(
            TRANSPORT_POSITION_PATH.into(),
            LeafValue::Number(0.0),
            5,
            true,
            &mut mixer_state,
            &bindings,
            &mut names,
            &mut commands,
        ));
        // An ordinary leaf still drives its view.
        assert!(apply_leaf(
            "mixer.channels.0.fader".into(),
            LeafValue::Number(-6.0),
            5,
            true,
            &mut mixer_state,
            &bindings,
            &mut names,
            &mut commands,
        ));
        params.apply(app.world_mut());
        app.update();

        assert_eq!(
            app.world().get::<ControlValue>(scrubber),
            Some(&ControlValue(42.0))
        );
        assert_eq!(
            app.world().get::<ControlValue>(fader),
            Some(&ControlValue(-6.0))
        );
        // State cache still accepted the leaf (readouts pre-first-poll use it).
        assert_eq!(
            mixer_state.value(TRANSPORT_POSITION_PATH),
            Some(&LeafValue::Number(0.0))
        );
    }

    #[test]
    fn busy_retry_is_superseded_by_queued_intent() {
        let path = "mixer.channels.0.fader";
        let mut io = MixerIo::default();
        // No queued intent: the busy write may retry.
        assert!(!busy_retry_superseded(&io, path));
        // Queued intent (stream latest / release / cancel baseline) wins.
        io.queued_latest
            .insert(path.into(), queued_number(0.0, 1, None));
        assert!(busy_retry_superseded(&io, path));
    }

    #[test]
    fn gesture_baseline_seeds_from_newest_pending_intent() {
        let path = "mixer.channels.0.fader";
        let mut io = MixerIo::default();
        let mut state = MusicdMixerState::default();

        // Nothing known: no baseline.
        assert_eq!(seed_gesture_baseline(&io, &state, path, None), None);

        // Server state is the fallback (floor = state revision).
        state.values.insert(path.into(), LeafValue::Number(-3.0));
        assert_eq!(
            seed_gesture_baseline(&io, &state, path, None),
            Some((LeafValue::Number(-3.0), None))
        );

        // A still-in-flight write (e.g. the previous release) outranks state,
        // and carries ITS OWN if_revision as the adoption floor — not current
        // state — so its CAS rejection can still update the baseline.
        io.pending.insert(
            7,
            RequestKind::Write {
                write: issued_number(path, -6.0, Some(4), 1, None),
                attempt: 0,
            },
        );
        assert_eq!(
            seed_gesture_baseline(&io, &state, path, None),
            Some((LeafValue::Number(-6.0), Some(4)))
        );

        // Queued outbox intent is the newest of all.
        io.queued_latest
            .insert(path.into(), queued_number(-9.0, 2, None));
        assert_eq!(
            seed_gesture_baseline(&io, &state, path, None),
            Some((LeafValue::Number(-9.0), None))
        );
    }

    #[test]
    fn external_changes_update_the_baseline_but_own_echoes_do_not() {
        let path = "mixer.channels.0.fader";
        let us = "ctk-mixer-test";
        let mut io = MixerIo::default();

        // No gesture: baseline updates are inactive.
        assert!(!external_change_updates_baseline(&io, path, 5, None, us));

        io.active_gestures
            .insert(path.into(), test_active_gesture(1));
        // source_id is definitive when present — even before our ack arrives.
        assert!(!external_change_updates_baseline(
            &io,
            path,
            5,
            Some(us),
            us
        ));
        assert!(external_change_updates_baseline(
            &io,
            path,
            5,
            Some("disp-skia-mixer"),
            us
        ));

        // Without a source (old daemon, snapshot leaves): revision fallback.
        assert!(external_change_updates_baseline(&io, path, 5, None, us));
        io.own_write_revisions
            .entry(path.into())
            .or_default()
            .insert(5);
        assert!(!external_change_updates_baseline(&io, path, 5, None, us));
        assert!(external_change_updates_baseline(&io, path, 6, None, us));
    }

    #[test]
    fn baseline_adoption_never_rewinds_past_its_revision_floor() {
        let path = "mixer.channels.0.fader";
        let us = "ctk-mixer-test";
        let mut io = MixerIo::default();
        io.active_gestures
            .insert(path.into(), test_active_gesture(1));
        io.gesture_baseline
            .insert(path.into(), (LeafValue::Number(0.0), Some(10)));

        // A delayed OLDER external event must not rewind the baseline.
        maybe_update_gesture_baseline(&mut io, path, 9, &LeafValue::Number(-3.0), None, us);
        assert_eq!(
            io.gesture_baseline.get(path),
            Some(&(LeafValue::Number(0.0), Some(10)))
        );

        // A NEWER external event advances it.
        maybe_update_gesture_baseline(&mut io, path, 11, &LeafValue::Number(-6.0), None, us);
        assert_eq!(
            io.gesture_baseline.get(path),
            Some(&(LeafValue::Number(-6.0), Some(11)))
        );
    }

    #[test]
    fn cancelled_gesture_writes_back_the_pre_gesture_baseline() {
        use crate::widgets::CtkWidgetsPlugin;

        let path = "mixer.channels.0.fader";
        let mut app = App::new();
        app.add_plugins(CtkWidgetsPlugin)
            .init_resource::<MusicdMixerState>()
            .init_resource::<MixerIo>()
            .add_observer(on_control_gesture_cancel);
        let control = app
            .world_mut()
            .spawn((
                knob(NumericControlProps::new(
                    "test.cancelled-fader",
                    0.9,
                    ControlRange {
                        min: -1.0,
                        max: 1.0,
                        step: 0.1,
                        detent: Some(0.0),
                    },
                    ValueMapping::linear(-1.0, 1.0).unwrap(),
                )),
                MixerBinding::number(path),
            ))
            .id();
        // Server state already holds a STREAMED mid-drag value; the baseline
        // is the pre-gesture authority cancel must return to.
        app.world_mut()
            .resource_mut::<MusicdMixerState>()
            .values
            .insert(path.into(), LeafValue::Number(-0.25));
        {
            let mut io = app.world_mut().resource_mut::<MixerIo>();
            io.active_gestures.insert(
                path.into(),
                ActiveGesture {
                    id: GestureId(1),
                    owner: control,
                },
            );
            io.queued_latest.insert(
                path.into(),
                QueuedWrite::new(
                    LeafValue::Number(0.9),
                    CommandMeta {
                        source: CommandSource::control(control),
                        gesture_id: Some(GestureId(1)),
                        generation: CommandGeneration(2),
                    },
                ),
            );
            io.gesture_baseline
                .insert(path.into(), (LeafValue::Number(0.5), Some(3)));
            io.retries.push(RetryWrite {
                due: Instant::now(),
                write: IssuedWrite {
                    request: WriteRequest {
                        path: path.into(),
                        value: LeafValue::Number(0.9),
                        op_id: "test-stale-stream".into(),
                        if_revision: None,
                    },
                    meta: CommandMeta {
                        source: CommandSource::control(control),
                        gesture_id: Some(GestureId(1)),
                        generation: CommandGeneration(3),
                    },
                },
                attempt: 1,
            });
        }

        app.world_mut()
            .trigger(ControlGestureCancel { source: control });
        app.update();

        let io = app.world().resource::<MixerIo>();
        assert!(!io.active_gestures.contains_key(path));
        assert!(!io.queued_latest.contains_key(path));
        assert!(!io.gesture_baseline.contains_key(path));
        // A Busy-retrying stream must not resurrect the cancelled value.
        assert!(io.retries.is_empty());
        // The view restores to the BASELINE, not the streamed server value.
        assert_eq!(
            app.world().get::<ControlValue>(control),
            Some(&ControlValue(0.5))
        );
    }

    #[test]
    fn overflow_invalidates_an_already_pending_snapshot() {
        let mut io = MixerIo::default();
        io.pending.insert(1, RequestKind::Snapshot);

        invalidate_inflight_snapshot(&mut io);

        assert!(snapshot_reply_needs_replacement(&io));
        assert!(io
            .pending
            .values()
            .any(|kind| matches!(kind, RequestKind::Snapshot)));
    }

    #[test]
    fn failed_periodic_snapshot_drops_only_already_applied_mirrors() {
        let changed = ChangedEvent {
            path: "mixer.channels.0.pan".into(),
            revision: 4,
            value: LeafValue::Number(0.25),
            source_id: None,
        };

        let mut ready_state = MusicdMixerState {
            ready: true,
            ..default()
        };
        let mut periodic_io = MixerIo::default();
        buffer_change(&mut periodic_io, changed.clone());
        handle_snapshot_failure(&mut ready_state, &mut periodic_io, "refresh failed".into());
        assert!(periodic_io.buffered_changes.is_empty());

        let mut bootstrap_state = MusicdMixerState::default();
        let mut bootstrap_io = MixerIo::default();
        buffer_change(&mut bootstrap_io, changed);
        handle_snapshot_failure(
            &mut bootstrap_state,
            &mut bootstrap_io,
            "bootstrap failed".into(),
        );
        assert_eq!(bootstrap_io.buffered_changes.len(), 1);
    }

    #[test]
    fn revision_epoch_reset_discards_old_authority_operations() {
        let path = "mixer.channels.0.fader".to_string();
        let request = WriteRequest {
            path: path.clone(),
            value: LeafValue::Number(-6.0),
            op_id: "old-op".into(),
            if_revision: Some(40),
        };
        let mut io = MixerIo::default();
        io.pending.insert(
            1,
            RequestKind::Write {
                write: IssuedWrite {
                    request: request.clone(),
                    meta: test_meta(1, None, "pending"),
                },
                attempt: 0,
            },
        );
        io.retries.push(RetryWrite {
            due: Instant::now(),
            write: IssuedWrite {
                request,
                meta: test_meta(2, None, "retry"),
            },
            attempt: 1,
        });
        io.inflight_paths.insert(path.clone());
        io.queued_latest
            .insert(path.clone(), queued_number(-3.0, 3, None));
        buffer_change(
            &mut io,
            ChangedEvent {
                path,
                revision: 40,
                value: LeafValue::Number(-6.0),
                source_id: None,
            },
        );

        reset_epoch_io(&mut io);

        assert!(io.pending.is_empty());
        assert!(io.retries.is_empty());
        assert!(io.inflight_paths.is_empty());
        assert!(io.queued_latest.is_empty());
        assert!(io.buffered_changes.is_empty());
    }
}
