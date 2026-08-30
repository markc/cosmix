//! The real 32-channel mixer Bus daemon (`cosmix-musicd mixer-serve`).
//!
//! This is the **DSP + telemetry** half of the disp-skia bake-off vertical
//! slice (`~/.cmctl/_decisions/2026-07-14-display-renderer-bakeoff.md` §8,
//! Codex GO thread 019f5f33). Where the `mixer-sim` subcommand runs the
//! [`crate::mixer::MixerEngine`] headless and prints frames, this subcommand
//! wires the *same* engine into the Bus mesh as a live, writeable mixer surface
//! and a telemetry publisher:
//!
//! 1. **`props.set` → revisioned control writes.** `musicd.props.set` carries a
//!    `mixer.v1` [`WriteRequest`]. Each write is validated
//!    ([`validate_write`]), applied to the opt-in in-memory
//!    [`RevWriteStore`](cosmix_props_core::revwrite::RevWriteStore) (server
//!    receive order authoritative, `if_revision` optimistic concurrency,
//!    per-path coalescing, terminal own-op echo), shipped to the RT thread over
//!    an `rtrb` control ring under a server-assigned monotonic revision, and
//!    acknowledged with a [`WriteAck`] (**control revision only** — Q8(a); the
//!    DSP application is reported later by `dsp.applied`).
//! 2. **`dsp.applied` events.** When the RT thread actually latches a revision
//!    into the audio graph it reports `{revision, sample_frame}` back over a
//!    return ring; the async side publishes it as [`DspApplied`] on
//!    `musicd.mixer.applied` (with the originating `path` as a header when
//!    known) so a bench can compute input→DSP latency.
//! 3. **60 Hz meter publisher.** The RT thread writes the latest encoded
//!    465-byte A.6 [`MeterFrame`] into a depth-1, latest-wins [`MeterMailbox`]
//!    (a wait-free seqlock — RT discipline); a 60 Hz task publishes it,
//!    base64-encoded, to `musicd.mixer.meters` (`retain=false`). Per-path
//!    control changes are separately published (coalesced) to
//!    `musicd.mixer.changed`.
//!
//! ## RT discipline
//!
//! The `"musicd-mixer"` thread mirrors `play.rs`: the hot path allocates
//! nothing. On a host with an audio device the engine is driven by a **real cpal
//! output stream** — the audio callback (owning the `!Send` stream on this
//! thread) pulls stereo from the engine in ≤128-frame internal blocks, clamps to
//! `[-1,1]` and writes the device buffer, so wall-clock pacing is the audio clock
//! itself and `frame0_mono` is pinned at the first callback. Only when there is no
//! device does the thread fall back to **software pacing** against
//! `CLOCK_MONOTONIC` (`frame0_mono`) with no audio output (flagged non-real-audio).
//! Either way control writes arrive over a pre-allocated `rtrb` SPSC ring, the
//! per-block meter frames land in a caller-owned pre-sized scratch `Vec`, and
//! frame publication is a `[u8; 465]` copy into the seqlock — no heap traffic,
//! no locks on the audio thread.
//!
//! ## props-core coupling (the assumed interface, for reconciliation)
//!
//! The revisioned facility used here is
//! [`cosmix_props_core::revwrite::RevWriteStore`] — a generic, `PropValue`-typed
//! store. This daemon is the domain layer on top: it parses/validates the
//! `mixer.v1` wire, converts [`LeafValue`] ↔ [`PropValue`], and maps the
//! store's generic [`RevWriteResponse`] back to the domain
//! [`WriteResponse`]. The coupling is exactly: `store.seed`, `store.apply`,
//! `store.get`, `store.path_revision`, `store.drain_changed` — nothing else.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use crate::rt_sched::{self, AudioRuntime, AudioRuntimeView, AudioWake, RT_PRIORITY_PENDING};
use anyhow::{Context as _, Result};
use base64::Engine as _;
use rtrb::{Consumer, RingBuffer};
use serde_json::Value as Json;
use std::os::fd::{AsRawFd, RawFd};
use tokio::io::unix::AsyncFd;
use tokio::sync::{Mutex, RwLock};
use tracing::{error, info, warn};

use cosmix_client::NodedClient;
use cosmix_mixer_schema::{
    DspApplied, LeafType, MeterFrame, MeterRecord, NUM_CHANNELS, WriteRequest, from_centi_dbfs,
    leaf_default, leaf_enum_values, leaf_spec,
};
use cosmix_props::PropValue;
use cosmix_props::{PropDescribe, PropPath, PropTree, PropType, tree::build_snapshot};
use cosmix_props_core::revwrite::{ChangedProp, RevWriteStore};

use crate::mixer::{Controls, SR, SongMeta, SourceProfile};
use crate::mixer_host::*;

/// Bus service identity. The mixer daemon answers `musicd.props.*` and publishes
/// the `musicd.mixer.*` topics. It replaces the MIDI-player `serve` identity for
/// a bake-off run — do not run both `serve` and `mixer-serve` on one broker.
const SERVICE: &str = "musicd";
/// Topic carrying the batched 60 Hz meter frames (base64 A.6 frame, retain=false).
pub const METERS_TOPIC: &str = "musicd.mixer.meters";
/// Topic carrying per-path coalesced control-change events (retain=false).
pub const CHANGED_TOPIC: &str = "musicd.mixer.changed";
/// Topic carrying `dsp.applied {revision, sample_frame}` latch events (retain=false).
pub const APPLIED_TOPIC: &str = "musicd.mixer.applied";

// ===========================================================================
// The read-side property snapshot (props.get / list / describe).
// ===========================================================================

/// A point-in-time `mixer.v1` property snapshot: current control state (from the
/// store) plus live meter values (from the latest meter frame).
struct MixerSnapshot {
    snap: PropValue,
}

impl PropTree for MixerSnapshot {
    fn snapshot(&self) -> PropValue {
        self.snap.clone()
    }
    fn list(&self) -> Vec<PropPath> {
        all_leaf_paths()
            .iter()
            .filter_map(|s| PropPath::new(s).ok())
            .collect()
    }
    fn describe(&self, path: &PropPath) -> Option<PropDescribe> {
        describe_leaf(path)
    }
}

fn meter_record_value(rec: &MeterRecord, field: &str) -> PropValue {
    let db = |c: i16| PropValue::Float(from_centi_dbfs(c));
    match field {
        "rms_l" => db(rec.rms_l),
        "rms_r" => db(rec.rms_r),
        "peak_l" => db(rec.peak_l),
        "peak_r" => db(rec.peak_r),
        "hold_l" => db(rec.hold_l),
        "hold_r" => db(rec.hold_r),
        "clip" => PropValue::Bool(rec.clip != 0),
        _ => PropValue::Null,
    }
}

fn snapshot_value_for(
    path: &str,
    store: &RevWriteStore,
    frame: Option<&MeterFrame>,
    transport_frames: u64,
    runtime: &AudioRuntimeView,
) -> PropValue {
    // Meter level + clip leaves reflect the latest frame (silence/false if none).
    if let Some((idx, field)) = meter_leaf(path) {
        // meter.clip is a control leaf below; only the LEVEL leaves + the live
        // clip readback come from the frame. `meter.clip` control state is the
        // latched clip bit, which is exactly the frame's clip.
        let default = MeterRecord::default();
        let rec = frame.and_then(|f| f.records.get(idx)).unwrap_or(&default);
        return meter_record_value(rec, field);
    }
    // transport.position is transient: report the LIVE RT transport position
    // (seconds), not the last-written seek target held in the store (M7).
    if path == "transport.position" {
        return PropValue::Float(transport_frames as f64 / SR as f64);
    }
    if path == RT_PRIORITY_PATH {
        return PropValue::Float(runtime.rt_priority as f64);
    }
    if path == BLOCK_FRAMES_PATH {
        return PropValue::Float(runtime.block_frames as f64);
    }
    if path == RT_TIME_US_PATH {
        return PropValue::Float(runtime.rt_time_us as f64);
    }
    // Control / transport / text leaves: the store's current value, else default.
    if let Ok(pp) = PropPath::new(path)
        && let Some(v) = store.get(&pp)
    {
        return v.clone();
    }
    leaf_default(path)
        .map(|v| leaf_to_prop(&v))
        .unwrap_or(PropValue::Null)
}

fn build_mixer_snapshot(
    ctl: &MixerCtl,
    frame: Option<&MeterFrame>,
    transport_frames: u64,
    runtime: &AudioRuntime,
) -> MixerSnapshot {
    let runtime = runtime.view();
    let leaves: Vec<(PropPath, PropValue)> = all_leaf_paths()
        .into_iter()
        .filter_map(|p| {
            let val = snapshot_value_for(&p, &ctl.store, frame, transport_frames, &runtime);
            PropPath::new(&p).ok().map(|pp| (pp, val))
        })
        .collect();
    MixerSnapshot {
        snap: build_snapshot(leaves),
    }
}

/// The display unit for a numeric leaf (format-as-unit convention).
fn unit_for(path: &str) -> Option<&'static str> {
    if path.ends_with(".pan") {
        return Some("unit-interval");
    }
    if path == "transport.position" || path == "transport.length" {
        return Some("seconds");
    }
    if let Some((_, field)) = meter_leaf(path) {
        return (field != "clip").then_some("dBFS");
    }
    // trim / fader / master.fader
    if path.ends_with(".trim") || path.ends_with(".fader") {
        return Some("dB");
    }
    None
}

fn describe_text(path: &str) -> String {
    if let Some((id, leaf)) = split_channel(path) {
        return match leaf {
            "trim" => format!("Channel {id} input trim."),
            "fader" => format!("Channel {id} fader level."),
            "pan" => format!("Channel {id} equal-power pan (-1 left .. +1 right)."),
            "mute" => format!("Channel {id} mute."),
            "solo" => format!("Channel {id} solo."),
            "name" => format!("Channel {id} display name."),
            "meter.clip" => format!("Channel {id} latched clip flag (write false to reset)."),
            _ => format!("Channel {id} meter level."),
        };
    }
    match path {
        "mixer.master.fader" => "Master fader level.".into(),
        "mixer.master.mute" => "Master mute.".into(),
        "mixer.master.meter.clip" => "Master latched clip flag (write false to reset).".into(),
        "transport.state" => "Transport state.".into(),
        "transport.position" => "Transport position in seconds.".into(),
        "transport.length" => "Total transport length in seconds (0 = unbounded).".into(),
        "mixer.song.title" => "Session song title (GUI footer; empty if unknown).".into(),
        "mixer.song.artist" => "Session song artist (GUI footer; empty if unknown).".into(),
        "mixer.song.copyright" => "Session song copyright (GUI footer; empty if unknown).".into(),
        "mixer.schema_version" => "Domain schema tag.".into(),
        "mixer.engine" => "Active engine (dsp | simulator).".into(),
        "mixer.source_profile" => {
            "Active source profile (benchmark-multitone.v1 | stem-session.v1).".into()
        }
        "mixer.benchmark_eligible" => {
            "Whether this run may enter a benchmark chart (true only for the benchmark profile)."
                .into()
        }
        RT_PRIORITY_PATH => {
            "Achieved audio-path SCHED_FIFO priority (-2 not yet observed, -1 refused, 0 disabled/unattempted, >0 applied).".into()
        }
        BLOCK_FRAMES_PATH => {
            "Maximum audio callback size observed in frames (0 before a callback or on paced no-output).".into()
        }
        RT_TIME_US_PATH => {
            "Soft RLIMIT_RTTIME guarding the audio thread, in microseconds (0 = no RT deadman switch armed).".into()
        }
        p if p.starts_with("mixer.master.meter.") => "Master meter level.".into(),
        _ => "mixer.v1 leaf.".into(),
    }
}

fn describe_leaf(path: &PropPath) -> Option<PropDescribe> {
    if path.as_str() == RT_PRIORITY_PATH {
        return Some(
            PropDescribe::leaf(path.clone(), PropType::Number, describe_text(path.as_str()))
                .with_mutable(false)
                .with_transient(true)
                .with_min(RT_PRIORITY_PENDING as f64)
                .with_max(99.0)
                .with_default(RT_PRIORITY_PENDING as f64),
        );
    }
    if path.as_str() == BLOCK_FRAMES_PATH {
        return Some(
            PropDescribe::leaf(path.clone(), PropType::Number, describe_text(path.as_str()))
                .with_mutable(false)
                .with_transient(true)
                .with_min(0.0)
                .with_max(u32::MAX as f64)
                .with_default(0.0),
        );
    }
    if path.as_str() == RT_TIME_US_PATH {
        return Some(
            PropDescribe::leaf(path.clone(), PropType::Number, describe_text(path.as_str()))
                .with_mutable(false)
                .with_transient(true)
                .with_min(0.0)
                .with_max(u32::MAX as f64)
                .with_default(0.0),
        );
    }
    let spec = leaf_spec(path.as_str())?;
    let ty = match spec.ty {
        LeafType::Number => PropType::Number,
        LeafType::Bool => PropType::Bool,
        // Enum + free text both surface as JSON strings on the read wire.
        LeafType::Enum | LeafType::Text => PropType::String,
    };
    let mut d = PropDescribe::leaf(path.clone(), ty, describe_text(path.as_str()))
        .with_mutable(spec.mutable)
        .with_transient(spec.transient);
    if spec.ty == LeafType::Number {
        d = d.with_min(spec.min).with_max(spec.max);
        if let Some(u) = unit_for(path.as_str()) {
            d = d.with_unit(u);
        }
    }
    if let Some(vals) = leaf_enum_values(path.as_str()) {
        d = d.with_enum(vals.iter().copied());
    }
    if let Some(def) = leaf_default(path.as_str()) {
        d = d.with_default(leaf_to_json(&def));
    }
    Some(d)
}

// ===========================================================================
// Bus command handling.
// ===========================================================================

fn json_error(msg: &str) -> String {
    serde_json::json!({ "error": msg }).to_string()
}

/// Parse the optional `args` for props.get/describe from header / args / body.
fn parse_args(cmd: &cosmix_client::IncomingCommand) -> Option<Json> {
    if let Some(s) = cmd.header("args")
        && let Ok(v) = serde_json::from_str::<Json>(s)
    {
        return Some(v);
    }
    if cmd.args.is_object() && !cmd.args.as_object().unwrap().is_empty() {
        return Some(cmd.args.clone());
    }
    if cmd.body.is_empty() {
        None
    } else {
        serde_json::from_str(&cmd.body).ok()
    }
}

async fn handle_set(
    ctl: &Arc<Mutex<MixerCtl>>,
    wake: &AudioWake,
    source_id: &str,
    body: &str,
) -> (u8, String) {
    let req: WriteRequest = match serde_json::from_str(body) {
        Ok(r) => r,
        Err(e) => return (RC_REJECT, json_error(&format!("invalid WriteRequest: {e}"))),
    };
    let (rc, response) = {
        let mut g = ctl.lock().await;
        let MixerCtl {
            store,
            controls,
            ctrl_tx,
            rev_path,
        } = &mut *g;
        // Reserve a slot (lower bound; we are the sole producer, so an observed
        // free slot stays free until we push) BEFORE applying the write.
        let ring_has_slot = ctrl_tx.slots() >= 1;
        let (rc, response, command) =
            apply_write(store, controls, rev_path, ring_has_slot, source_id, req);
        // Ship the command inside the lock so store order == ring order. The
        // reservation above guarantees this push cannot fail (accepted → enqueued);
        // a failure here would be a logic bug, not a dropped-under-load write.
        if let Some(cmd) = command
            && ctrl_tx.push(cmd).is_err()
        {
            error!("BUG: mixer control ring full after slot reservation — accepted write lost");
        }
        (rc, response)
    };
    // The write landed in the store, so `props.changed` has something to drain.
    // Wake the publisher instead of letting it wait out the position cadence —
    // this is the async-side producer of the same queue the RT thread feeds.
    // rc 0 is accept; RC_BUSY / RC_REJECT changed nothing.
    if rc == 0 {
        wake.signal();
    }
    (
        rc,
        serde_json::to_string(&response).unwrap_or_else(|_| json_error("serialise")),
    )
}

async fn command_loop(client: Arc<NodedClient>, ctl: Arc<Mutex<MixerCtl>>, shared: Shared) {
    let mut rx = match client.incoming_async().await {
        Some(rx) => rx,
        None => return,
    };

    while let Some(cmd) = rx.recv().await {
        // props.watch → the topics + the snapshot verb to bootstrap from.
        if cmd.command == "musicd.props.watch" || cmd.command == "props.watch" {
            let body = serde_json::json!({
                "topic": CHANGED_TOPIC,
                "meters_topic": METERS_TOPIC,
                "applied_topic": APPLIED_TOPIC,
                "snapshot": "musicd.mixer.snapshot",
                "info": "Subscribe first, then read musicd.mixer.snapshot and discard any \
                         changed event whose revision <= the snapshot revision.",
            })
            .to_string();
            if let Err(e) = client.respond(&cmd, 0, &body).await {
                error!("props.watch respond: {e}");
            }
            continue;
        }

        // mixer.snapshot → the revisioned bootstrap snapshot (MAJOR 6) + the
        // run-integrity fault flags benchd polls throughout the run.
        if cmd.command == "musicd.mixer.snapshot" || cmd.command == "mixer.snapshot" {
            let real_audio = shared.real_audio.load(Ordering::Acquire);
            let audio_fault = shared.audio_fault.load(Ordering::Acquire);
            let applied_fault = shared.applied_fault.load(Ordering::Acquire);
            let resp = {
                let g = ctl.lock().await;
                build_snapshot_response(
                    &g,
                    &shared.audio_runtime,
                    real_audio,
                    audio_fault,
                    applied_fault,
                    shared.source_profile,
                    shared.benchmark_eligible,
                )
            };
            let body = serde_json::to_string(&resp).unwrap_or_else(|_| json_error("serialise"));
            if let Err(e) = client.respond(&cmd, 0, &body).await {
                error!("mixer.snapshot respond: {e}");
            }
            continue;
        }

        // props.set → a revisioned mixer.v1 control write.
        if cmd.command == "musicd.props.set" || cmd.command == "props.set" {
            // Authenticated source = broker-verified sender identity ONLY (MAJOR 8):
            // no "anonymous" fallback — an unauthenticated write is rejected by
            // apply_write (empty source_id).
            let source_id = cmd.from.as_str();
            let body = if !cmd.body.is_empty() {
                cmd.body.clone()
            } else if cmd.args.is_object() {
                cmd.args.to_string()
            } else {
                String::new()
            };
            let (rc, resp) = handle_set(&ctl, &shared.wake, source_id, &body).await;
            if let Err(e) = client.respond(&cmd, rc, &resp).await {
                error!("props.set respond: {e}");
            }
            continue;
        }

        // props.get / list / describe → the read snapshot.
        if let Some(suffix) = cmd.command.strip_prefix("musicd.props.") {
            let frame = shared
                .meters
                .read()
                .and_then(|b| MeterFrame::decode(&b).ok());
            let transport_frames = shared.transport_pos.load(Ordering::Relaxed);
            let snapshot = {
                let g = ctl.lock().await;
                build_mixer_snapshot(&g, frame.as_ref(), transport_frames, &shared.audio_runtime)
            };
            let args = parse_args(&cmd);
            let resp = cosmix_props::bus::dispatch_props(&snapshot, suffix, args.as_ref(), true);
            let rc_u8: u8 = resp.rc.clamp(0, 255) as u8;
            if let Err(e) = client.respond(&cmd, rc_u8, &resp.body).await {
                error!("props respond: {e}");
            }
            continue;
        }

        // Unknown command.
        if let Err(e) = client
            .respond(
                &cmd,
                RC_REJECT,
                &json_error(&format!("unknown command: {}", cmd.command)),
            )
            .await
        {
            error!("respond: {e}");
        }
    }

    info!("broker connection closed");
}

// ===========================================================================
// Long-lived publishers (meters + events), decoupled from reconnects.
// ===========================================================================

/// A slot holding the current broker client (or `None` while disconnected). The
/// publishers read it each tick, so they survive reconnects without re-owning
/// their `rtrb` consumers / mailbox.
type ClientSlot = Arc<RwLock<Option<Arc<NodedClient>>>>;

/// RT → async telemetry the command loop reads: the meter mailbox, the live
/// transport position, whether real cpal audio is active, and the two sticky
/// run-integrity faults (audio lost mid-run / applied-backlog overflow). Cheap to
/// clone (all `Arc`).
#[derive(Clone)]
struct Shared {
    meters: Arc<MeterMailbox>,
    transport_pos: Arc<AtomicU64>,
    /// RT/async wake shared with the event publisher: signalled when a write
    /// lands so `props.changed` is published without waiting for a tick.
    wake: Arc<AudioWake>,
    real_audio: Arc<AtomicBool>,
    audio_fault: Arc<AtomicBool>,
    applied_fault: Arc<AtomicBool>,
    /// Live callback scheduling and buffer-size telemetry for read-only props.
    audio_runtime: Arc<AudioRuntime>,
    /// The immutable active source-profile id (`mixer.source_profile`), chosen at
    /// startup. `&'static str` — one of the schema profile-id consts.
    source_profile: &'static str,
    /// Whether this run is benchmark-eligible (`mixer.benchmark_eligible`; true
    /// only for the benchmark profile). Immutable at runtime.
    benchmark_eligible: bool,
}

async fn publish_topic(
    client: &NodedClient,
    topic: &str,
    body: &str,
    extra_path: Option<&str>,
) -> Result<()> {
    // Wrap the payload in a valid Bus message before publishing. noded's
    // subscription.publish parses the inner body AS Bus and re-delivers it, so a
    // raw base64/JSON body (no `---` frontmatter) is rejected as MalformedPayload
    // and never reaches a subscriber. The inner message carries the topic verb as
    // `command`, the payload as its body, and the optional `path` header the
    // subscriber reads — mirroring world.rs's `build_*_message().to_wire()`.
    let mut inner =
        cosmix_bus::bus::BusMessage::command([("command", topic.to_string())]).with_body(body);
    if let Some(p) = extra_path {
        inner.set("path", p);
    }
    let inner_wire = inner.to_wire();
    let mut headers = BTreeMap::new();
    headers.insert("name".to_string(), topic.to_string());
    headers.insert("retain".to_string(), "false".to_string());
    client
        .send_with_headers("noded", "topic.publish", &headers, &inner_wire)
        .await
}

/// Transport-position publish cadence. A genuine sampling rate for a value that
/// moves continuously (the RT thread advances it every block), not a poll for
/// work — the drains it shares a loop with are wake-driven.
const POSITION_HZ: u64 = 10;
const POSITION_PERIOD: Duration = Duration::from_millis(1000 / POSITION_HZ);

/// Newtype so tokio's reactor can own a borrow of the shared [`AudioWake`].
/// `AsyncFd` needs `AsRawFd`, which cannot be implemented for `Arc<AudioWake>`
/// from here; holding the `Arc` also guarantees the fd outlives the registration.
pub struct WakeFd(pub Arc<AudioWake>);

impl AsRawFd for WakeFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

/// 60 Hz meter publisher: the latest A.6 frame, base64, to `musicd.mixer.meters`.
async fn meter_loop(slot: ClientSlot, meters: Arc<MeterMailbox>) {
    let period = Duration::from_nanos(1_000_000_000 / 60);
    let mut ticker = tokio::time::interval(period);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        let client = { slot.read().await.clone() };
        let Some(client) = client else { continue };
        if let Some(bytes) = meters.read() {
            let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
            if let Err(e) = publish_topic(&client, METERS_TOPIC, &b64, None).await {
                warn!("meter publish failed: {e}");
            }
        }
    }
}

/// Event publisher: drains `dsp.applied` (revision latched) and coalesced
/// `props.changed`, publishing to the applied / changed topics, plus a ~10 Hz
/// `transport.position` progress publish so a subscriber (the mixer scrubber)
/// tracks playback + seeks live — the RT thread advances the position atomic
/// every block, but nothing else pushes it onto `mixer.changed`.
///
/// **Woken, not polled** (`_decisions/2026-07-20-no-poll-event-driven-amp-wake.md`).
/// This loop used to run a 2 ms (500 Hz) ticker whose only job was to notice work
/// on a ring. It now sleeps on an `eventfd` the RT thread signals after it pushes,
/// and which the command path signals after a write lands. The one remaining
/// timer is the 10 Hz transport-position publish, which is a genuine sampling
/// cadence for a continuously-moving value — not a poll for work — and doubles as
/// the backstop if a wake is ever missed.
async fn event_loop(
    slot: ClientSlot,
    ctl: Arc<Mutex<MixerCtl>>,
    mut applied_rx: Consumer<AppliedMsg>,
    transport_pos: Arc<AtomicU64>,
    afd: AsyncFd<WakeFd>,
) {
    // If the readiness registration ever faults, stop selecting on it and let
    // the position cadence drive the drains: degraded latency, never a stall,
    // and never a busy loop on a broken fd.
    let mut wake_broken = false;
    let mut ticker = tokio::time::interval(POSITION_PERIOD);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Transport-position publish state. `last_pos_frame` gates on change (silent
    // when stopped/idle). Each position frame is published at the CURRENT global
    // store revision — NOT a running counter (MAJOR): a monotonic `+1`-per-tick
    // counter grew unbounded (~10/s), so after a musicd restart the renderer
    // kept the old huge `xe` revision and stale-rejected the fresh publisher
    // (starting near 0) for a span PROPORTIONAL TO PRIOR PLAYTIME. The renderer's
    // stale-guard rejects `revision < existing` (EQUAL is accepted), so all
    // position frames sharing the constant store revision during playback are
    // accepted (latest value wins); the revision only climbs when a real write
    // bumps the store. Bounded → the restart gap is now just the small write
    // count, never playtime. ~10 Hz = every 50th 2 ms tick.
    let mut last_pos_frame = transport_pos.load(Ordering::Relaxed);
    loop {
        // Either the RT thread (or a landed write) signalled work, or the
        // position cadence came round. Both paths fall through to the same
        // drains, so a wake never has to be paired with a matching tick.
        let mut publish_position = false;
        tokio::select! {
            r = afd.readable(), if !wake_broken => match r {
                Ok(mut g) => {
                    // Clear BEFORE draining: tokio's registration is
                    // edge-triggered, so a signal that lands after this point
                    // re-arms readiness rather than being swallowed by the read
                    // we are about to do.
                    g.clear_ready();
                    afd.get_ref().0.drain();
                }
                Err(e) => {
                    error!(
                        "event_loop: wake fd readiness failed ({e}); publishing at the \
                         {POSITION_HZ} Hz transport cadence only from here on"
                    );
                    // WONTFIX: the 10 Hz fallback can fill the applied ring plus
                    // backlog if registration fails, a device accepts a
                    // pathologically tiny callback, and writes advance on every
                    // callback. The integrity fault is the designed response:
                    // disqualify the run rather than corrupt a true latch frame.
                    wake_broken = true;
                }
            },
            _ = ticker.tick() => publish_position = true,
        }
        let client = { slot.read().await.clone() };

        // dsp.applied: drain the return ring (even while disconnected, so it
        // never backs up). Each ring message carries a high-water revision + the
        // block's sample frame; the async side **expands EVERY pending revision
        // <= the high-water** into its own `dsp.applied {revision, sample_frame}`
        // (+ path header) and prunes it — so no applied notification is ever
        // dropped, and benchd can correlate each individual write (MAJOR 4). Even
        // if the RT ring momentarily filled and skipped a block's push, a later
        // block's >= high-water expansion still covers every earlier revision.
        while let Ok(msg) = applied_rx.pop() {
            let expanded: Vec<(u64, PropPath)> = {
                let mut g = ctl.lock().await;
                drain_applied(&mut g.rev_path, msg.revision)
            };
            if let Some(client) = &client {
                for (revision, path) in expanded {
                    let dsp = DspApplied {
                        revision,
                        sample_frame: msg.sample_frame,
                    };
                    let body = serde_json::to_string(&dsp).unwrap();
                    if let Err(e) =
                        publish_topic(client, APPLIED_TOPIC, &body, Some(path.as_str())).await
                    {
                        warn!("dsp.applied publish failed: {e}");
                    }
                }
            }
        }

        // props.changed: drain the coalesced control-change set.
        let changed: Vec<ChangedProp> = {
            let mut g = ctl.lock().await;
            g.store.drain_changed()
        };
        if let Some(client) = &client {
            for ch in changed {
                let body = serde_json::json!({
                    "path": ch.path.as_str(),
                    "revision": ch.revision,
                    "value": Json::from(&ch.canonical_value),
                    "source_id": ch.source_id,
                    "op_id": ch.op_id,
                })
                .to_string();
                if let Err(e) =
                    publish_topic(client, CHANGED_TOPIC, &body, Some(ch.path.as_str())).await
                {
                    warn!("props.changed publish failed: {e}");
                }
            }
        }

        // ~10 Hz transport-position progress: publish only when the frame moved
        // (silent when stopped/idle), on the SAME changed topic + path so the
        // bridge relays it to the display handle exactly like a control change.
        if publish_position {
            let frame = transport_pos.load(Ordering::Relaxed);
            if frame != last_pos_frame {
                last_pos_frame = frame;
                // KNOWN LIMITATION (restart revision epoch — future work, do NOT
                // implement now): the renderer stale-guards `xe` by revision, so
                // after a musicd restart while disp-skia keeps running, the
                // renderer retains the old (now write-count-bounded) revision and
                // rejects this restarted publisher's lower revisions until enough
                // new writes occur (or disp-skia restarts). Inherent to
                // revision-reconcile across a restarting authority; the proper fix
                // is a session/epoch id in `ui.value` that resets per-handle
                // revisions when it changes.
                // Publish at the current store revision (bounded — see above).
                let pub_rev = { ctl.lock().await.store.revision() };
                if let Some(client) = &client {
                    let body = serde_json::json!({
                        "path": "transport.position",
                        "revision": pub_rev,
                        "value": frame as f64 / SR as f64,
                        "source_id": "engine",
                        "op_id": "",
                    })
                    .to_string();
                    if let Err(e) =
                        publish_topic(client, CHANGED_TOPIC, &body, Some("transport.position"))
                            .await
                    {
                        warn!("transport.position publish failed: {e}");
                    }
                }
            }
        }
    }
}

// ===========================================================================
// Entry point.
// ===========================================================================

/// Run the real mixer Bus daemon: spawn the RT engine thread + the long-lived
/// meter/event publishers, then a supervised reconnect loop serving
/// `musicd.props.*`.
pub async fn serve(autoplay: bool, stems: Option<PathBuf>) -> Result<()> {
    let _log = cosmix_log::init(
        &cosmix_log::LogOpts::default(),
        &cosmix_log::StatsOpts::default(),
        cosmix_log::LogDefaults::daemon("cosmix-musicd").with_stats(false),
    );

    // Select the immutable source profile ONCE at startup. `--stems` loads,
    // verifies, and preloads the whole manifest into a StemBank (all filesystem +
    // hash + decode + allocation happens here, before cpal); absent = the frozen
    // benchmark multitone profile (default). The profile is fixed for the process
    // lifetime, so a musical run can never be mistaken for a benchmark run.
    let profile = match stems.as_deref() {
        Some(path) => {
            let bank = load_stem_bank(path)?;
            info!(
                "mixer-serve: STEM-SESSION profile — {} stem(s), {} logical frames, from {}",
                bank.loaded_channels(),
                bank.length_frames(),
                path.display()
            );
            warn!(
                "mixer-serve: stem-session source is NON-BENCHMARK — every meter frame \
                 carries FLAG_NON_BENCH_SOURCE and mixer.benchmark_eligible=false"
            );
            SourceProfile::StemSession(bank)
        }
        None => SourceProfile::BenchmarkMultitone,
    };
    // Captured before `profile` moves into the RT engine (both Copy).
    let source_profile_id = profile.id();
    // `--autoplay` starts RT transport outside the authoritative snapshot path (a
    // demo/test trigger), so a run using it can never be a certified benchmark —
    // regardless of source determinism. Force ineligible (finding #9, Mark's
    // call): fail closed so an accidental `--autoplay` cannot pass the
    // eligibility axes and slip a non-authoritative run into a ranked chart.
    let benchmark_eligible = profile.benchmark_eligible() && !autoplay;

    // Capture the GUI-facing per-stem instrument names + total length in seconds
    // + session song metadata BEFORE `profile` moves into the RT engine, to seed
    // the read-only mixer.channels.N.name + transport.length + mixer.song.* leaves
    // below. Multitone has no names/song and is unbounded (length 0.0).
    let (stem_names, transport_length_secs, song): ([Option<String>; NUM_CHANNELS], f64, SongMeta) =
        match &profile {
            SourceProfile::StemSession(bank) => (
                bank.names().clone(),
                bank.length_frames() as f64 / SR as f64,
                bank.song().clone(),
            ),
            // The daemon has no --song launch path (yet) — but the variant
            // must still seed sane leaves if one ever reaches serve(), and
            // the match must stay exhaustive for feature combos that compile
            // the synth profile in (this arm was missed when MidiSynth
            // landed; --all-features did not build).
            SourceProfile::MidiSynth(bank) => (
                bank.names().clone(),
                bank.length_frames() as f64 / SR as f64,
                bank.song().clone(),
            ),
            SourceProfile::BenchmarkMultitone => {
                (std::array::from_fn(|_| None), 0.0, SongMeta::default())
            }
        };

    // RT plumbing: control ring (async→RT), applied ring (RT→async), meter mailbox,
    // live transport position, the real-audio flag, and the two sticky
    // run-integrity faults.
    let (ctrl_tx, ctrl_rx) = RingBuffer::<RtCommand>::new(1024);
    let (applied_tx, applied_rx) = RingBuffer::<AppliedMsg>::new(256);
    let meters = Arc::new(MeterMailbox::new());
    let transport_pos = Arc::new(AtomicU64::new(0));
    let real_audio = Arc::new(AtomicBool::new(false));
    let audio_fault = Arc::new(AtomicBool::new(false));
    let applied_fault = Arc::new(AtomicBool::new(false));
    let audio_runtime = Arc::new(AudioRuntime::new(rt_sched::configured_rt_priority()));
    // RT → async wake. Created before the RT thread so the engine can signal from
    // its very first block; registration failure here is fatal on purpose — an
    // eventfd musicd cannot register is a broken host, not a degraded mode.
    let applied_wake = Arc::new(AudioWake::new().context("create the RT wake eventfd")?);
    let wake_afd =
        AsyncFd::new(WakeFd(applied_wake.clone())).context("register the RT wake eventfd")?;
    let rt = RtState::<MailboxSink>::new(
        ctrl_rx,
        applied_tx,
        MailboxSink(meters.clone()),
        transport_pos.clone(),
        applied_fault.clone(),
        profile,
    )
    .with_applied_wake(applied_wake.clone());
    let _rt = spawn_rt_thread(
        rt,
        real_audio.clone(),
        audio_fault.clone(),
        audio_runtime.clone(),
    );
    info!(
        "musicd-mixer RT engine started (32ch @ {} Hz, dsp mode, source_profile={})",
        SR, source_profile_id
    );
    let shared = Shared {
        meters,
        transport_pos,
        wake: applied_wake.clone(),
        real_audio,
        audio_fault,
        applied_fault,
        audio_runtime,
        source_profile: source_profile_id,
        benchmark_eligible,
    };

    // Seed the revisioned store with the mixer.v1 defaults at revision 0.
    let mut store = RevWriteStore::new();
    seed_store(
        &mut store,
        &stem_names,
        transport_length_secs,
        &song,
        source_profile_id,
        benchmark_eligible,
    );
    let ctl = Arc::new(Mutex::new(MixerCtl {
        store,
        controls: Controls::default(),
        ctrl_tx,
        rev_path: BTreeMap::new(),
    }));

    // `--autoplay` (demo/test only): start the RT playing at unity immediately,
    // so the daemon drives the audio device without an external authenticated
    // write. This seeds the RT engine only (the snapshot still reflects the
    // Bus-written transport state); it is never used for a measured benchmark run.
    if autoplay {
        let mut g = ctl.lock().await;
        g.controls.playing = true;
        let controls = g.controls;
        let _ = g.ctrl_tx.push(RtCommand::SetControls {
            controls,
            revision: 0,
        });
        warn!("mixer-serve --autoplay: transport PLAYING at unity (demo/test only, non-benchmark)");
    }

    // Long-lived publishers, decoupled from the broker connection lifetime.
    let slot: ClientSlot = Arc::new(RwLock::new(None));
    tokio::spawn(meter_loop(slot.clone(), shared.meters.clone()));
    tokio::spawn(event_loop(
        slot.clone(),
        ctl.clone(),
        applied_rx,
        shared.transport_pos.clone(),
        wake_afd,
    ));

    // Provenance built once so started_at is the true process start.
    let bi = cosmix_buildinfo::build_info!();
    let prov = cosmix_bus::RegisterProvenance::from_parts(
        bi.pkg,
        bi.version,
        bi.git_sha,
        bi.git_dirty,
        bi.build_time,
        cosmix_buildinfo::now_rfc3339(),
    );

    // Supervised reconnect loop. The RT engine + cpal stream are independent of
    // the broker connection, so audio keeps playing across reconnects.
    let mut backoff = Duration::from_secs(1);
    let mut ever_registered = false;
    loop {
        match cosmix_config::client_helpers::connect_default_with_provenance(SERVICE, prov.clone())
            .await
        {
            Ok(client) => {
                info!("registered as Bus service '{SERVICE}' (mixer)");
                ever_registered = true;
                backoff = Duration::from_secs(1);
                let client = Arc::new(client);
                *slot.write().await = Some(client.clone());
                command_loop(client, ctl.clone(), shared.clone()).await;
                *slot.write().await = None;
                warn!("broker connection closed; reconnecting");
            }
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("already registered") {
                    // MAJOR 9: a name collision with a DIFFERENT daemon at STARTUP
                    // is fatal — mixer-serve and the MIDI 'serve' daemon share the
                    // '{SERVICE}' identity and must not run together. But once we
                    // have successfully registered at least once, an "already
                    // registered" on RECONNECT is our OWN prior registration that
                    // the broker has not yet evicted after the drop — a transient
                    // self-reconnect race, NOT a foreign collision. Retry (the RT
                    // engine keeps sounding), never fail fast, or a single broker
                    // blip would kill a live session.
                    if ever_registered {
                        warn!(
                            "'{SERVICE}' still shows our prior registration after a reconnect; \
                             retrying in {backoff:?} (broker eviction of the stale entry)"
                        );
                    } else {
                        error!(
                            "FATAL: another '{SERVICE}' service is already registered on this \
                             broker. mixer-serve replaces the MIDI 'serve' daemon for a bake-off \
                             run — stop the other daemon first. Exiting."
                        );
                        return Err(anyhow::anyhow!(
                            "service '{SERVICE}' already registered: {msg}"
                        ));
                    }
                } else {
                    info!("broker unavailable; retrying in {backoff:?}: {e}");
                }
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmix_mixer_schema::{self as mixer, CENTI_DB_MIN, LeafValue};

    fn fresh() -> (RevWriteStore, Controls, BTreeMap<u64, PropPath>) {
        let mut store = RevWriteStore::new();
        for path in seed_leaves() {
            if let (Ok(pp), Some(def)) = (PropPath::new(&path), leaf_default(&path)) {
                store.seed(pp, leaf_to_prop(&def), "default");
            }
        }
        (store, Controls::default(), BTreeMap::new())
    }

    fn wreq(path: &str, v: LeafValue, op: &str, if_rev: Option<u64>) -> WriteRequest {
        WriteRequest {
            path: path.into(),
            value: v,
            op_id: op.into(),
            if_revision: if_rev,
        }
    }

    #[test]
    fn describe_carries_range_default_enum_and_unit() {
        let d = describe_leaf(&PropPath::new("mixer.channels.0.fader").unwrap()).unwrap();
        assert!(d.mutable);
        assert_eq!(d.min, Some(mixer::FADER_MIN_DB));
        assert_eq!(d.max, Some(mixer::FADER_MAX_DB));
        assert_eq!(d.format.as_deref(), Some("dB"));
        assert_eq!(d.default, Some(serde_json::json!(0.0)));

        let ts = describe_leaf(&PropPath::new("transport.state").unwrap()).unwrap();
        assert_eq!(
            ts.enum_values.as_deref(),
            Some(&["stopped".to_string(), "playing".to_string()][..])
        );

        let rms = describe_leaf(&PropPath::new("mixer.channels.0.meter.rms_l").unwrap()).unwrap();
        assert!(!rms.mutable);
        assert!(rms.transient);
        assert_eq!(rms.format.as_deref(), Some("dBFS"));

        for path in [RT_PRIORITY_PATH, BLOCK_FRAMES_PATH, RT_TIME_US_PATH] {
            let runtime = describe_leaf(&PropPath::new(path).unwrap()).unwrap();
            assert!(!runtime.mutable, "{path} must be read-only");
            assert!(runtime.transient, "{path} is live runtime telemetry");
            let expected_default = if path == RT_PRIORITY_PATH {
                RT_PRIORITY_PENDING as f64
            } else {
                0.0
            };
            assert_eq!(runtime.default, Some(serde_json::json!(expected_default)));
        }
        let rt = describe_leaf(&PropPath::new(RT_PRIORITY_PATH).unwrap()).unwrap();
        assert!(rt.description.contains("-2 not yet observed"));
    }

    #[test]
    fn snapshot_reflects_writes_and_meter_frame() {
        let (mut store, mut controls, mut rp) = fresh();
        apply_write(
            &mut store,
            &mut controls,
            &mut rp,
            true,
            "s",
            wreq("mixer.channels.2.pan", LeafValue::Number(0.5), "op", None),
        );
        let ctl = MixerCtl {
            store,
            controls,
            ctrl_tx: RingBuffer::<RtCommand>::new(1).0,
            rev_path: rp,
        };

        // A live frame with channel 2 carrying a clip + a level.
        let mut f = MeterFrame {
            seq: 0,
            capture_frame: 800,
            applied_rev: 1,
            frame0_mono: 0,
            flags: 0,
            records: [MeterRecord::default(); mixer::NUM_METERS],
        };
        f.records[2].rms_l = mixer::to_centi_dbfs(-12.0);
        f.records[2].clip = 0b01;

        // 96_000 frames of transport = 2.0 s at 48 kHz (live transient read).
        let runtime = AudioRuntime::new(0);
        runtime.prime_from_callback(512);
        let snap = build_mixer_snapshot(&ctl, Some(&f), 96_000, &runtime);
        let pan = snap
            .get(&PropPath::new("mixer.channels.2.pan").unwrap())
            .unwrap();
        assert_eq!(pan, PropValue::Float(0.5));
        let rms = snap
            .get(&PropPath::new("mixer.channels.2.meter.rms_l").unwrap())
            .unwrap();
        assert_eq!(rms, PropValue::Float(-12.0));
        let clip = snap
            .get(&PropPath::new("mixer.channels.2.meter.clip").unwrap())
            .unwrap();
        assert_eq!(clip, PropValue::Bool(true));
        // transport.position reflects the LIVE RT clock, not the store.
        let pos = snap
            .get(&PropPath::new("transport.position").unwrap())
            .unwrap();
        assert_eq!(pos, PropValue::Float(2.0));
        // A channel with no frame data reads silence.
        let silent = snap
            .get(&PropPath::new("mixer.channels.9.meter.rms_l").unwrap())
            .unwrap();
        assert_eq!(silent, PropValue::Float(from_centi_dbfs(CENTI_DB_MIN)));
        assert_eq!(
            snap.get(&PropPath::new(BLOCK_FRAMES_PATH).unwrap())
                .unwrap(),
            PropValue::Float(512.0)
        );
        assert_eq!(
            snap.get(&PropPath::new(RT_PRIORITY_PATH).unwrap()).unwrap(),
            PropValue::Float(0.0)
        );
    }
}
