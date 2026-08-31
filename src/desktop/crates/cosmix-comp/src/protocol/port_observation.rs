//! Calloop-owned semantic observation reduction and the bounded Bus outbox.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use chrono::{SecondsFormat, TimeZone, Utc};
use cosmix_bus::bus::BusMessage;
use crossbeam_channel::{Receiver, Sender, TryRecvError, TrySendError};
use serde::Serialize;
use serde_json::{Value, json};
use smithay::output::Output;
use smithay::reexports::calloop::{
    LoopHandle, RegistrationToken,
    timer::{TimeoutAction, Timer},
};
use smithay::reexports::wayland_server::Resource;
use tokio::sync::Notify;

use crate::port::{ControlReply, PortControl, PortSetRequest};

use super::{
    SurfaceId, WaylandState,
    corner::{Corner, CornerConfig, CornerDetector, CornerEvent},
    port_snapshot::{
        BindingRowSnapshot, CompSnapshot, FocusSnapshot, LayerSnapshot, OutputSnapshot,
        SurfaceSnapshot, WindowSnapshot, project_focus, project_output, project_outputs,
        project_stack, project_surface_by_id, project_window_row, snapshot,
    },
};

pub(crate) const PROPS_TOPIC_SUFFIX: &str = "props.changed";
pub(crate) const SURFACE_MAPPED_TOPIC_SUFFIX: &str = "surface.mapped";
pub(crate) const SURFACE_UNMAPPED_TOPIC_SUFFIX: &str = "surface.unmapped";
pub(crate) const FOCUS_TOPIC_SUFFIX: &str = "focus.changed";
pub(crate) const OUTPUT_TOPIC_SUFFIX: &str = "output.changed";
pub(crate) const CORNER_ENTERED_TOPIC_SUFFIX: &str = "corner.entered";
pub(crate) const CORNER_LEFT_TOPIC_SUFFIX: &str = "corner.left";

pub(crate) fn topic_name(service: &str, suffix: &str) -> String {
    format!("{service}.{suffix}")
}

type PendingPropChanges = BTreeMap<String, (PropValue, PropValue, &'static str)>;

const OUTBOX_CAPACITY: usize = 256;
// One interval may precede the publisher's held record while another follows
// it. Two slots preserve that boundary without allocating or rebuilding data.
const MARKER_CAPACITY: usize = 2;

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(untagged)]
pub(crate) enum PropValue {
    Null(()),
    Bool(bool),
    U64(u64),
    I32(i32),
    U32(u32),
    F32(f32),
    F64(f64),
    String(String),
    U64List(Vec<u64>),
    BindingRows(Vec<BindingRowSnapshot>),
    OutputRow(Box<OutputSnapshot>),
    SurfaceRow(Box<SurfaceSnapshot>),
    WindowRow(Box<WindowSnapshot>),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum SetValidationError {
    UnknownPath,
    ReadOnly,
    InvalidValue {
        path: String,
        expected: &'static str,
        range: &'static str,
    },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum ValidatedCornerValue {
    Enabled(bool),
    DeadzonePx(f64),
    DwellMs(u64),
    VelocityMaxPxS(f64),
}

impl PropValue {
    fn null() -> Self {
        Self::Null(())
    }

    pub(crate) fn wire_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ObservationRecord {
    PropsChanged {
        path: String,
        old: PropValue,
        new: PropValue,
        unix_ms: i64,
        cause: &'static str,
        event_seq: u64,
    },
    SurfaceMapped {
        id: u64,
        role: String,
        foreign_id: Option<String>,
        event_seq: u64,
    },
    SurfaceUnmapped {
        id: u64,
        role: String,
        foreign_id: Option<String>,
        event_seq: u64,
    },
    FocusChanged {
        keyboard: Option<u64>,
        previous: Option<u64>,
        exclusive_latch: Option<u64>,
        event_seq: u64,
    },
    OutputChanged {
        output: String,
        row: OutputSnapshot,
        event_seq: u64,
    },
    CornerEntered {
        output: String,
        corner: Corner,
        dwell_ms: u64,
        event_seq: u64,
    },
    CornerLeft {
        output: String,
        corner: Corner,
        dwell_ms: u64,
        event_seq: u64,
    },
}

impl ObservationRecord {
    pub(crate) fn event_seq(&self) -> u64 {
        match self {
            Self::PropsChanged { event_seq, .. }
            | Self::SurfaceMapped { event_seq, .. }
            | Self::SurfaceUnmapped { event_seq, .. }
            | Self::FocusChanged { event_seq, .. }
            | Self::OutputChanged { event_seq, .. }
            | Self::CornerEntered { event_seq, .. }
            | Self::CornerLeft { event_seq, .. } => *event_seq,
        }
    }

    pub(crate) fn topic_suffix(&self) -> &'static str {
        match self {
            Self::PropsChanged { .. } => PROPS_TOPIC_SUFFIX,
            Self::SurfaceMapped { .. } => SURFACE_MAPPED_TOPIC_SUFFIX,
            Self::SurfaceUnmapped { .. } => SURFACE_UNMAPPED_TOPIC_SUFFIX,
            Self::FocusChanged { .. } => FOCUS_TOPIC_SUFFIX,
            Self::OutputChanged { .. } => OUTPUT_TOPIC_SUFFIX,
            Self::CornerEntered { .. } => CORNER_ENTERED_TOPIC_SUFFIX,
            Self::CornerLeft { .. } => CORNER_LEFT_TOPIC_SUFFIX,
        }
    }

    pub(crate) fn wire(&self) -> BusMessage {
        let mut message = BusMessage::new();
        message.set("command", self.topic_suffix());
        message.set("event_seq", &self.event_seq().to_string());
        message.body = match self {
            Self::PropsChanged {
                path,
                old,
                new,
                unix_ms,
                cause,
                event_seq,
            } => {
                message.set("path", path);
                message.set("cause", cause);
                json!({
                    "path": path,
                    "old": old.wire_value(),
                    "new": new.wire_value(),
                    "ts": rfc3339_millis(*unix_ms),
                    "cause": cause,
                    "event_seq": event_seq,
                })
                .to_string()
            }
            Self::SurfaceMapped {
                id,
                role,
                foreign_id,
                event_seq,
            }
            | Self::SurfaceUnmapped {
                id,
                role,
                foreign_id,
                event_seq,
            } => {
                let mut body = json!({
                    "id": id,
                    "role": role,
                    "event_seq": event_seq,
                });
                if let Some(foreign_id) = foreign_id {
                    body.as_object_mut()
                        .expect("surface event body is an object")
                        .insert("foreign_id".into(), json!(foreign_id));
                }
                body.to_string()
            }
            Self::FocusChanged {
                keyboard,
                previous,
                exclusive_latch,
                event_seq,
            } => json!({
                "keyboard": keyboard,
                "previous": previous,
                "exclusive_latch": exclusive_latch,
                "event_seq": event_seq,
            })
            .to_string(),
            Self::OutputChanged {
                output,
                row,
                event_seq,
            } => json!({
                "output": output,
                "geometry": {
                    "x": row.x,
                    "y": row.y,
                    "width": row.width,
                    "height": row.height,
                },
                "usable": row.usable,
                "event_seq": event_seq,
            })
            .to_string(),
            Self::CornerEntered {
                output,
                corner,
                dwell_ms,
                event_seq,
            }
            | Self::CornerLeft {
                output,
                corner,
                dwell_ms,
                event_seq,
            } => json!({
                "output": output,
                "corner": corner.name(),
                "dwell_ms": dwell_ms,
                "event_seq": event_seq,
            })
            .to_string(),
        };
        message
    }
}

const TOPIC_SUFFIXES: [&str; 7] = [
    PROPS_TOPIC_SUFFIX,
    SURFACE_MAPPED_TOPIC_SUFFIX,
    SURFACE_UNMAPPED_TOPIC_SUFFIX,
    FOCUS_TOPIC_SUFFIX,
    OUTPUT_TOPIC_SUFFIX,
    CORNER_ENTERED_TOPIC_SUFFIX,
    CORNER_LEFT_TOPIC_SUFFIX,
];

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct AffectedTopics(u8);

impl AffectedTopics {
    fn index(suffix: &str) -> usize {
        TOPIC_SUFFIXES
            .iter()
            .position(|candidate| *candidate == suffix)
            .expect("every observation has one of the seven fixed topic suffixes")
    }

    pub(crate) fn insert(&mut self, suffix: &str) {
        let index = Self::index(suffix);
        self.0 |= 1 << index;
    }

    pub(crate) fn merge(&mut self, other: Self) {
        self.0 |= other.0;
    }

    pub(crate) fn remove(&mut self, suffix: &str) {
        self.0 &= !(1 << Self::index(suffix));
    }

    pub(crate) fn iter(self) -> impl Iterator<Item = &'static str> {
        TOPIC_SUFFIXES
            .into_iter()
            .enumerate()
            .filter_map(move |(index, suffix)| (self.0 & (1 << index) != 0).then_some(suffix))
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum LossCause {
    OutboxOverflow,
    PublisherLoss,
}

impl LossCause {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::OutboxOverflow => "outbox.overflow",
            Self::PublisherLoss => "publisher.loss",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct LossInterval {
    pub(crate) first_lost_seq: u64,
    pub(crate) last_lost_seq: u64,
    pub(crate) topics: AffectedTopics,
    pub(crate) cause: LossCause,
}

impl LossInterval {
    pub(crate) fn from_record(record: &ObservationRecord, cause: LossCause) -> Self {
        let mut topics = AffectedTopics::default();
        topics.insert(record.topic_suffix());
        Self {
            first_lost_seq: record.event_seq(),
            last_lost_seq: record.event_seq(),
            topics,
            cause,
        }
    }

    pub(crate) fn merge(&mut self, other: Self) {
        self.first_lost_seq = self.first_lost_seq.min(other.first_lost_seq);
        self.last_lost_seq = self.last_lost_seq.max(other.last_lost_seq);
        self.topics.merge(other.topics);
        self.cause = self.cause.max(other.cause);
    }
}

pub(crate) struct ObservationOutbox {
    pub(crate) records: Receiver<ObservationRecord>,
    pub(crate) markers: Receiver<LossInterval>,
    pub(crate) marker_update_generation: Arc<AtomicU64>,
    pub(crate) held_record_seq: Arc<AtomicU64>,
}

fn rfc3339_millis(unix_ms: i64) -> String {
    Utc.timestamp_millis_opt(unix_ms)
        .single()
        .unwrap_or_else(Utc::now)
        .to_rfc3339_opts(SecondsFormat::Millis, true)
}

pub(crate) struct ObservationProducer {
    record_sender: Sender<ObservationRecord>,
    record_eviction: Receiver<ObservationRecord>,
    marker_sender: Sender<LossInterval>,
    marker_merge: Receiver<LossInterval>,
    pending_marker: Option<LossInterval>,
    marker_update_generation: Arc<AtomicU64>,
    held_record_seq: Arc<AtomicU64>,
    lost_count: Arc<AtomicU64>,
    notifier: Arc<Notify>,
}

impl ObservationProducer {
    /// Fixed-cost calloop boundary: bounded channels allocate their storage at
    /// construction. One offer performs at most two marker flushes, two data
    /// sends, one data eviction and a two-slot marker normalisation; it must
    /// never grow a Vec/VecDeque/Box, serialise JSON, wait, poll, lock or loop
    /// over either queue. The two atomics only arbitrate the independent lanes.
    pub(crate) fn offer(&mut self, record: ObservationRecord) {
        self.flush_pending_marker();
        match self.record_sender.try_send(record) {
            Ok(()) => self.notifier.notify_one(),
            Err(TrySendError::Disconnected(_)) => {
                self.lost_count.fetch_add(1, Ordering::AcqRel);
            }
            Err(TrySendError::Full(record)) => {
                self.replace_one_oldest(record);
            }
        }
    }

    fn replace_one_oldest(&mut self, record: ObservationRecord) {
        match self.record_eviction.try_recv() {
            Ok(evicted) => {
                self.merge_pending_marker(LossInterval::from_record(
                    &evicted,
                    LossCause::OutboxOverflow,
                ));
                self.lost_count.fetch_add(1, Ordering::AcqRel);
                self.flush_pending_marker();
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.lost_count.fetch_add(1, Ordering::AcqRel);
                return;
            }
        }

        match self.record_sender.try_send(record) {
            Ok(()) => self.notifier.notify_one(),
            Err(TrySendError::Disconnected(_)) => {
                self.lost_count.fetch_add(1, Ordering::AcqRel);
            }
            Err(TrySendError::Full(_)) => {
                unreachable!("one consumer or one eviction leaves one data slot")
            }
        }
    }

    fn merge_pending_marker(&mut self, marker: LossInterval) {
        if let Some(pending) = self.pending_marker.as_mut() {
            pending.merge(marker);
        } else {
            self.pending_marker = Some(marker);
        }
    }

    fn flush_pending_marker(&mut self) {
        let Some(marker) = self.pending_marker.take() else {
            return;
        };
        match self.marker_sender.try_send(marker) {
            Ok(()) => self.notifier.notify_one(),
            Err(TrySendError::Disconnected(marker)) => {
                self.pending_marker = Some(marker);
            }
            Err(TrySendError::Full(marker)) => self.normalise_full_marker_lane(marker),
        }
    }

    fn normalise_full_marker_lane(&mut self, marker: LossInterval) {
        // The marker lane must never look empty to the publisher while its
        // occupant is being merged. The publisher observes this fixed-cost
        // critical section and waits on Notify; no lock or spin is involved.
        self.marker_update_generation.fetch_add(1, Ordering::AcqRel);

        let first = self.marker_merge.try_recv().ok();
        let second = self.marker_merge.try_recv().ok();
        let held = self.held_record_seq.load(Ordering::Acquire);
        let mut before: Option<LossInterval> = None;
        let mut after: Option<LossInterval> = None;
        for candidate in [first, second, Some(marker)].into_iter().flatten() {
            let target = if held == 0
                || candidate.last_lost_seq < held
                || candidate.first_lost_seq <= held
            {
                &mut before
            } else {
                &mut after
            };
            self.merge_marker_slot(target, candidate);
        }

        let connected = self.finish_marker_flush(before) && self.finish_marker_flush(after);
        self.marker_update_generation
            .fetch_add(1, Ordering::Release);
        self.notifier.notify_one();
        if !connected {
            if let Some(marker) = before {
                self.merge_pending_marker(marker);
            }
            if let Some(marker) = after {
                self.merge_pending_marker(marker);
            }
        }
    }

    fn merge_marker_slot(&self, slot: &mut Option<LossInterval>, marker: LossInterval) {
        if let Some(current) = slot.as_mut() {
            current.merge(marker);
        } else {
            *slot = Some(marker);
        }
    }

    fn finish_marker_flush(&self, marker: Option<LossInterval>) -> bool {
        let Some(marker) = marker else {
            return true;
        };
        match self.marker_sender.try_send(marker) {
            Ok(()) => true,
            Err(TrySendError::Disconnected(_)) => false,
            Err(TrySendError::Full(_)) => {
                unreachable!("two fixed marker slots were drained before normalisation")
            }
        }
    }

    pub(crate) fn notifier(&self) -> Arc<Notify> {
        Arc::clone(&self.notifier)
    }
}

pub(crate) fn outbox(lost_count: Arc<AtomicU64>) -> (ObservationProducer, ObservationOutbox) {
    outbox_with_capacity(lost_count, OUTBOX_CAPACITY)
}

fn outbox_with_capacity(
    lost_count: Arc<AtomicU64>,
    capacity: usize,
) -> (ObservationProducer, ObservationOutbox) {
    assert!(capacity > 0, "observation data lane must have capacity");
    let (record_sender, records) = crossbeam_channel::bounded(capacity);
    let (marker_sender, markers) = crossbeam_channel::bounded(MARKER_CAPACITY);
    let notifier = Arc::new(Notify::new());
    let marker_update_generation = Arc::new(AtomicU64::new(0));
    let held_record_seq = Arc::new(AtomicU64::new(0));
    (
        ObservationProducer {
            record_sender,
            record_eviction: records.clone(),
            marker_sender,
            marker_merge: markers.clone(),
            pending_marker: None,
            marker_update_generation: Arc::clone(&marker_update_generation),
            held_record_seq: Arc::clone(&held_record_seq),
            lost_count,
            notifier,
        },
        ObservationOutbox {
            records,
            markers,
            marker_update_generation,
            held_record_seq,
        },
    )
}

#[cfg(test)]
pub(crate) fn test_outbox(
    lost_count: Arc<AtomicU64>,
    capacity: usize,
) -> (ObservationProducer, ObservationOutbox) {
    outbox_with_capacity(lost_count, capacity)
}

#[derive(Clone, Debug)]
struct SurfaceEdgeStart {
    mapped: bool,
    role: String,
    foreign_id: Option<String>,
}

#[derive(Clone, Debug)]
struct OutputEdgeStart {
    output: Output,
    row: OutputSnapshot,
}

#[derive(Clone, Copy, Debug)]
struct FocusEdgeStart {
    keyboard: Option<u64>,
    exclusive_latch: Option<u64>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct CornerRegion {
    pub(crate) key_index: usize,
    pub(crate) origin: (f64, f64),
    pub(crate) size: (f64, f64),
}

pub(crate) struct ObservationState {
    pending_surface_edges: BTreeMap<u64, SurfaceEdgeStart>,
    dirty_surfaces: BTreeMap<u64, &'static str>,
    dirty_outputs: BTreeMap<String, OutputEdgeStart>,
    output_topology_dirty: bool,
    property_dirty_outputs: BTreeMap<String, (Output, &'static str)>,
    pending_focus: Option<FocusEdgeStart>,
    focus_cause: &'static str,
    stack_dirty: Option<&'static str>,
    full_dirty: Option<&'static str>,
    watched_baseline: Option<CompSnapshot>,
    pub(crate) corner_config: CornerConfig,
    pub(crate) corner_regions: Vec<CornerRegion>,
    pub(crate) corner_output_keys: Vec<String>,
    corner_detector: CornerDetector,
    corner_output: Option<usize>,
    corner_clock: Instant,
    corner_timer: Option<RegistrationToken>,
    corner_timer_deadline_ms: Option<u64>,
    #[cfg(test)]
    corner_timer_arms: usize,
    loop_handle: LoopHandle<'static, WaylandState>,
    producer: ObservationProducer,
    event_seq: u64,
    event_seq_exhausted: bool,
    event_seq_watermark: Arc<AtomicU64>,
}

impl ObservationState {
    pub(super) fn new(
        producer: ObservationProducer,
        event_seq_watermark: Arc<AtomicU64>,
        loop_handle: LoopHandle<'static, WaylandState>,
    ) -> Self {
        let corner_config = CornerConfig::default();
        Self {
            pending_surface_edges: BTreeMap::new(),
            dirty_surfaces: BTreeMap::new(),
            dirty_outputs: BTreeMap::new(),
            output_topology_dirty: false,
            property_dirty_outputs: BTreeMap::new(),
            pending_focus: None,
            focus_cause: "wayland.focus",
            stack_dirty: None,
            full_dirty: None,
            watched_baseline: None,
            corner_config,
            corner_regions: Vec::new(),
            corner_output_keys: Vec::new(),
            corner_detector: CornerDetector::new(corner_config, (0.0, 0.0)),
            corner_output: None,
            corner_clock: Instant::now(),
            corner_timer: None,
            corner_timer_deadline_ms: None,
            #[cfg(test)]
            corner_timer_arms: 0,
            loop_handle,
            producer,
            event_seq: 0,
            event_seq_exhausted: false,
            event_seq_watermark,
        }
    }

    fn next_seq(&mut self) -> Option<u64> {
        if self.event_seq_exhausted {
            return None;
        }
        self.event_seq += 1;
        self.event_seq_exhausted = self.event_seq == u64::MAX;
        self.event_seq_watermark
            .store(self.event_seq, Ordering::Release);
        Some(self.event_seq)
    }

    fn offer(&mut self, build: impl FnOnce(u64) -> ObservationRecord) -> Option<u64> {
        let sequence = self.next_seq()?;
        self.producer.offer(build(sequence));
        Some(sequence)
    }

    pub(crate) fn drop_watch(&mut self) {
        self.watched_baseline = None;
    }
}

impl WaylandState {
    pub(crate) fn mark_surface_before_change(&mut self, id: SurfaceId) {
        if self.observations.pending_surface_edges.contains_key(&id.0) {
            return;
        }
        let Some(object) = self.surface_objects.get(&id) else {
            return;
        };
        let Some(record) = self.surfaces.get(object) else {
            return;
        };
        self.observations.pending_surface_edges.insert(
            id.0,
            SurfaceEdgeStart {
                mapped: record.mapped,
                role: record.role.kind().to_string(),
                foreign_id: self.foreign_toplevel_identifiers.get(&id).cloned(),
            },
        );
    }

    pub(crate) fn mark_surface_mapped(
        &mut self,
        surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    ) {
        let Some((id, was_mapped)) = self
            .surfaces
            .get(&surface.id())
            .map(|record| (record.id, record.mapped))
        else {
            return;
        };
        if !was_mapped {
            self.mark_surface_before_change(id);
        }
        self.mark_surface_dirty(id, "wayland.map");
        if !was_mapped {
            self.mark_stack_dirty("wayland.map");
        }
    }

    pub(crate) fn mark_surface_unmapped(
        &mut self,
        surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    ) {
        let Some((id, was_mapped)) = self
            .surfaces
            .get(&surface.id())
            .map(|record| (record.id, record.mapped))
        else {
            return;
        };
        if was_mapped {
            self.mark_surface_before_change(id);
        }
        self.mark_surface_dirty(id, "wayland.unmap");
        if was_mapped {
            self.mark_stack_dirty("wayland.unmap");
        }
    }

    pub(crate) fn mark_surface_dirty(&mut self, id: SurfaceId, cause: &'static str) {
        if self.observations.watched_baseline.is_none() {
            return;
        }
        self.observations
            .dirty_surfaces
            .entry(id.0)
            .or_insert(cause);
    }

    pub(crate) fn mark_stack_dirty(&mut self, cause: &'static str) {
        if self.observations.watched_baseline.is_none() {
            return;
        }
        self.observations.stack_dirty.get_or_insert(cause);
    }

    pub(crate) fn mark_focus_before_change(&mut self, cause: &'static str) {
        if self.observations.pending_focus.is_none() {
            self.observations.pending_focus = Some(project_focus_edge(self));
        }
        if self.observations.watched_baseline.is_none() {
            return;
        }
        self.observations.focus_cause = cause;
    }

    pub(crate) fn mark_output_before_change(&mut self, output: &Output, cause: &'static str) {
        let Some((key, row)) = project_output(self, output) else {
            if self.observations.watched_baseline.is_some() {
                self.observations.full_dirty.get_or_insert(cause);
            }
            return;
        };
        self.observations
            .dirty_outputs
            .entry(key.clone())
            .or_insert_with(|| OutputEdgeStart {
                output: output.clone(),
                row,
            });
        if self.observations.watched_baseline.is_none() {
            return;
        }
        self.observations
            .property_dirty_outputs
            .entry(key)
            .or_insert_with(|| (output.clone(), cause));
    }

    #[cfg(any(all(feature = "kms-live", not(test)), test))]
    pub(crate) fn mark_all_outputs_before_change(&mut self, cause: &'static str) {
        let Some(projection) = project_outputs(self) else {
            if self.observations.watched_baseline.is_some() {
                self.observations.full_dirty.get_or_insert(cause);
            }
            return;
        };
        for (output, key) in projection.keys {
            let Some(row) = projection.rows.get(&key).cloned() else {
                continue;
            };
            self.observations
                .property_dirty_outputs
                .entry(key.clone())
                .or_insert((output.clone(), cause));
            self.observations
                .dirty_outputs
                .entry(key)
                .or_insert(OutputEdgeStart { output, row });
        }
    }

    #[cfg(any(all(feature = "kms-live", not(test)), test))]
    pub(crate) fn mark_output_topology_before_change(&mut self) {
        self.mark_all_outputs_before_change("output.geometry");
        self.observations.output_topology_dirty = true;
        self.observations
            .full_dirty
            .get_or_insert("output.geometry");
    }

    pub(crate) fn mark_session_observation_dirty(&mut self) {
        self.mark_focus_before_change("session.lock");
        self.observations.full_dirty.get_or_insert("session.lock");
    }

    pub(crate) fn emit_corner_entered(&mut self, output: String, corner: Corner, dwell_ms: u64) {
        self.observations
            .offer(|event_seq| ObservationRecord::CornerEntered {
                output,
                corner,
                dwell_ms,
                event_seq,
            });
    }

    pub(crate) fn emit_corner_left(&mut self, output: String, corner: Corner, dwell_ms: u64) {
        self.observations
            .offer(|event_seq| ObservationRecord::CornerLeft {
                output,
                corner,
                dwell_ms,
                event_seq,
            });
    }

    pub(crate) fn refresh_corner_regions(&mut self) {
        let Some(projection) = project_outputs(self) else {
            self.observations.corner_regions.clear();
            self.reset_corner_detector();
            self.observations.corner_output_keys.clear();
            return;
        };
        let mut keys = Vec::with_capacity(projection.keys.len());
        let mut regions = Vec::with_capacity(projection.keys.len());
        for (_, key) in projection.keys {
            let Some(row) = projection.rows.get(&key) else {
                continue;
            };
            let key_index = keys.len();
            keys.push(key);
            regions.push(CornerRegion {
                key_index,
                origin: (f64::from(row.x), f64::from(row.y)),
                size: (f64::from(row.width), f64::from(row.height)),
            });
        }
        self.observations.corner_output_keys = keys;
        self.observations.corner_regions = regions;
    }

    pub(crate) fn sample_corner_motion(
        &mut self,
        position: (f64, f64),
        region_hint: usize,
        attempted_motion: (f64, f64),
    ) {
        if self.session_lock_active() {
            self.reset_corner_detector();
            return;
        }
        let contains = |region: &CornerRegion| {
            position.0 >= region.origin.0
                && position.0 < region.origin.0 + region.size.0
                && position.1 >= region.origin.1
                && position.1 < region.origin.1 + region.size.1
        };
        let region = self
            .observations
            .corner_regions
            .get(region_hint)
            .copied()
            .filter(contains)
            .or_else(|| {
                self.observations
                    .corner_regions
                    .iter()
                    .copied()
                    .find(contains)
            });
        let Some(region) = region else {
            self.reset_corner_detector();
            return;
        };
        if self.observations.corner_output != Some(region.key_index) {
            self.reset_corner_detector();
            self.observations.corner_output = Some(region.key_index);
            let config = self.observations.corner_config;
            let events = self
                .observations
                .corner_detector
                .reconfigure(config, region.size);
            self.emit_corner_events(events, region.key_index);
        }
        let local = (position.0 - region.origin.0, position.1 - region.origin.1);
        let at_ms =
            u64::try_from(self.observations.corner_clock.elapsed().as_millis()).unwrap_or(u64::MAX);
        let events = self
            .observations
            .corner_detector
            .sample(at_ms, local, attempted_motion);
        self.emit_corner_events(events, region.key_index);
        self.rearm_corner_timer();
    }

    pub(crate) fn apply_corner_config(&mut self, config: CornerConfig) {
        let output = self.observations.corner_output;
        let size = output
            .and_then(|index| self.observations.corner_regions.get(index))
            .map_or((0.0, 0.0), |region| region.size);
        self.observations.corner_config = config;
        let events = self.observations.corner_detector.reconfigure(config, size);
        if let Some(output) = output {
            self.emit_corner_events(events, output);
        }
        self.rearm_corner_timer();
    }

    pub(crate) fn reset_corner_detector(&mut self) {
        let output = self.observations.corner_output.take();
        let events = self.observations.corner_detector.reset();
        if let Some(output) = output {
            self.emit_corner_events(events, output);
        }
        if let Some(token) = self.observations.corner_timer.take() {
            self.observations.loop_handle.remove(token);
        }
        self.observations.corner_timer_deadline_ms = None;
    }

    fn emit_corner_events(&mut self, events: [Option<CornerEvent>; 2], output_index: usize) {
        for event in events.into_iter().flatten() {
            let Some(output) = self
                .observations
                .corner_output_keys
                .get(output_index)
                .cloned()
            else {
                continue;
            };
            match event {
                CornerEvent::Entered { corner, dwell_ms } => {
                    self.emit_corner_entered(output, corner, dwell_ms);
                }
                CornerEvent::Left { corner, dwell_ms } => {
                    self.emit_corner_left(output, corner, dwell_ms);
                }
            }
        }
    }

    fn rearm_corner_timer(&mut self) {
        let deadline = self.observations.corner_detector.next_deadline_ms();
        if deadline == self.observations.corner_timer_deadline_ms
            && self.observations.corner_timer.is_some()
        {
            return;
        }
        if let Some(token) = self.observations.corner_timer.take() {
            self.observations.loop_handle.remove(token);
        }
        self.observations.corner_timer_deadline_ms = None;
        let Some(deadline_ms) = deadline else {
            return;
        };
        let now_ms =
            u64::try_from(self.observations.corner_clock.elapsed().as_millis()).unwrap_or(u64::MAX);
        let delay = Duration::from_millis(deadline_ms.saturating_sub(now_ms));
        self.observations.corner_timer = self
            .observations
            .loop_handle
            .insert_source(Timer::from_duration(delay), |_, _, state| {
                state.observations.corner_timer = None;
                state.observations.corner_timer_deadline_ms = None;
                let position = state.cursor_position;
                let region_index = state.observations.corner_output.unwrap_or_default();
                state.sample_corner_motion(position, region_index, (0.0, 0.0));
                TimeoutAction::Drop
            })
            .ok();
        if self.observations.corner_timer.is_some() {
            self.observations.corner_timer_deadline_ms = Some(deadline_ms);
            #[cfg(test)]
            {
                self.observations.corner_timer_arms =
                    self.observations.corner_timer_arms.saturating_add(1);
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn corner_timer_probe(&mut self) -> (Option<u64>, usize) {
        self.rearm_corner_timer();
        (
            self.observations.corner_timer_deadline_ms,
            self.observations.corner_timer_arms,
        )
    }

    #[cfg(test)]
    pub(crate) fn corner_candidate_position_probe(&self) -> Option<(f64, f64)> {
        self.observations.corner_detector.candidate_position()
    }
}

pub(super) fn service_observations(state: &mut WaylandState) {
    let stable =
        state.pointer_hit_test_batch_depth == 0 && !state.pointer_hit_test_transaction_applying;
    debug_assert!(
        stable,
        "Bus observation attempted inside a protocol transaction or hit-test batch"
    );
    if !stable {
        return;
    }
    service_surface_edges(state);
    service_focus_edge(state);
    service_output_edges(state);
    service_property_diffs(state);
    service_controls(state);
}

fn service_surface_edges(state: &mut WaylandState) {
    let pending = std::mem::take(&mut state.observations.pending_surface_edges);
    for (raw_id, old) in pending {
        let id = SurfaceId(raw_id);
        let final_record = state
            .surface_objects
            .get(&id)
            .and_then(|object| state.surfaces.get(object));
        let final_mapped = final_record.is_some_and(|record| record.mapped);
        if old.mapped == final_mapped {
            continue;
        }
        let role = if final_mapped {
            final_record
                .map(|record| record.role.kind().to_string())
                .unwrap_or(old.role)
        } else {
            old.role
        };
        let foreign_id = state
            .foreign_toplevel_identifiers
            .get(&id)
            .cloned()
            .or(old.foreign_id);
        state.observations.offer(|event_seq| {
            if final_mapped {
                ObservationRecord::SurfaceMapped {
                    id: raw_id,
                    role,
                    foreign_id,
                    event_seq,
                }
            } else {
                ObservationRecord::SurfaceUnmapped {
                    id: raw_id,
                    role,
                    foreign_id,
                    event_seq,
                }
            }
        });
    }
}

fn service_focus_edge(state: &mut WaylandState) {
    let Some(previous) = state.observations.pending_focus.take() else {
        return;
    };
    let current = project_focus_edge(state);
    if previous.keyboard == current.keyboard && previous.exclusive_latch == current.exclusive_latch
    {
        return;
    }
    if let Some(id) = previous.keyboard {
        state.mark_surface_dirty(SurfaceId(id), "wayland.focus");
    }
    if let Some(id) = current.keyboard {
        state.mark_surface_dirty(SurfaceId(id), "wayland.focus");
    }
    state
        .observations
        .offer(|event_seq| ObservationRecord::FocusChanged {
            keyboard: current.keyboard,
            previous: previous.keyboard,
            exclusive_latch: current.exclusive_latch,
            event_seq,
        });
}

fn project_focus_edge(state: &WaylandState) -> FocusEdgeStart {
    FocusEdgeStart {
        keyboard: state
            .keyboard
            .current_focus()
            .as_ref()
            .and_then(|surface| state.surfaces.get(&surface.id()))
            .map(|record| record.id.0),
        exclusive_latch: state
            .exclusive_keyboard_focus
            .as_ref()
            .and_then(|object| state.surfaces.get(object))
            .map(|record| record.id.0),
    }
}

fn service_output_edges(state: &mut WaylandState) {
    let pending = std::mem::take(&mut state.observations.dirty_outputs);
    let topology_dirty = std::mem::take(&mut state.observations.output_topology_dirty);
    if pending.is_empty() && !topology_dirty {
        return;
    }
    let final_rows = if topology_dirty {
        let Some(projection) = project_outputs(state) else {
            state.observations.dirty_outputs = pending;
            state.observations.output_topology_dirty = true;
            if state.observations.watched_baseline.is_some() {
                state
                    .observations
                    .full_dirty
                    .get_or_insert("output.geometry");
            }
            return;
        };
        projection.rows
    } else {
        BTreeMap::new()
    };
    let old_keys = pending.keys().cloned().collect::<BTreeSet<_>>();
    let final_keys = final_rows.keys().cloned().collect::<BTreeSet<_>>();
    let topology_replaced = topology_dirty && old_keys != final_keys;
    if topology_replaced {
        state.reset_corner_detector();
    }
    let mut emitted = BTreeSet::new();
    let mut retry = BTreeMap::new();
    for (key, old) in pending {
        let row = if topology_dirty {
            final_rows.get(&key).cloned()
        } else {
            project_output(state, &old.output)
                .and_then(|(final_key, row)| (final_key == key).then_some(row))
        };
        let Some(row) = row else {
            if state.backend.port_output(&old.output).is_some() {
                retry.insert(key, old);
            }
            continue;
        };
        if old.row.x == row.x
            && old.row.y == row.y
            && old.row.width == row.width
            && old.row.height == row.height
            && old.row.usable == row.usable
        {
            continue;
        }
        state.reset_corner_detector();
        state
            .observations
            .offer(|event_seq| ObservationRecord::OutputChanged {
                output: key.clone(),
                row,
                event_seq,
            });
        emitted.insert(key);
    }
    state.observations.dirty_outputs.extend(retry);
    if topology_replaced {
        for (key, row) in final_rows {
            if old_keys.contains(&key) || emitted.contains(&key) {
                continue;
            }
            state.reset_corner_detector();
            state
                .observations
                .offer(|event_seq| ObservationRecord::OutputChanged {
                    output: key,
                    row,
                    event_seq,
                });
        }
    }
    state.refresh_corner_regions();
}

fn service_property_diffs(state: &mut WaylandState) {
    let Some(mut baseline) = state.observations.watched_baseline.take() else {
        state.observations.dirty_surfaces.clear();
        state.observations.property_dirty_outputs.clear();
        state.observations.stack_dirty = None;
        state.observations.full_dirty = None;
        state.observations.focus_cause = "wayland.focus";
        return;
    };
    let mut changes = PendingPropChanges::new();
    let full_cause = state.observations.full_dirty.take();
    if let Some(cause) = full_cause {
        let Some(context) = state.port_context.clone() else {
            state.observations.full_dirty = Some(cause);
            state.observations.watched_baseline = Some(baseline);
            return;
        };
        let Some(next) = snapshot(state, &context) else {
            state.observations.full_dirty = Some(cause);
            state.observations.watched_baseline = Some(baseline);
            return;
        };
        collect_snapshot_diff(&baseline, &next, cause, &mut changes);
        baseline = next;
        state.observations.dirty_surfaces.clear();
        state.observations.property_dirty_outputs.clear();
        state.observations.stack_dirty = None;
        state.observations.focus_cause = "wayland.focus";
        flush_prop_changes(state, changes);
        state.observations.watched_baseline = Some(baseline);
        return;
    }

    let dirty_outputs = std::mem::take(&mut state.observations.property_dirty_outputs);
    for (key, (output, cause)) in dirty_outputs {
        let old = baseline.outputs.get(&key).cloned();
        let new = project_output(state, &output)
            .filter(|(final_key, _)| final_key == &key)
            .map(|(_, row)| row);
        if new.is_none() && state.backend.port_output(&output).is_some() {
            state.observations.full_dirty.get_or_insert(cause);
            continue;
        }
        diff_output_row(
            &format!("outputs.{key}"),
            old.as_ref(),
            new.as_ref(),
            cause,
            &mut changes,
        );
        match new {
            Some(row) => {
                baseline.outputs.insert(key, row);
            }
            None => {
                baseline.outputs.remove(&key);
            }
        }
    }
    let dirty_surfaces = std::mem::take(&mut state.observations.dirty_surfaces);
    if !dirty_surfaces.is_empty() {
        if let Some(projection) = project_outputs(state) {
            for (raw_id, cause) in dirty_surfaces {
                let key = format!("s{raw_id}");
                let old_surface = baseline.surfaces.get(&key).cloned();
                let new_surface = project_surface_by_id(state, SurfaceId(raw_id), &projection.keys);
                diff_surface_row(
                    &format!("surfaces.{key}"),
                    old_surface.as_ref(),
                    new_surface.as_ref(),
                    cause,
                    &mut changes,
                );
                match new_surface {
                    Some(row) => {
                        baseline.surfaces.insert(key.clone(), row.clone());
                        if row.role == "toplevel" && row.mapped && !state.session_lock_active() {
                            let old_window = baseline.windows.get(&key).cloned();
                            let new_window = project_window_row(&row);
                            diff_window_row(
                                &format!("windows.{key}"),
                                old_window.as_ref(),
                                Some(&new_window),
                                cause,
                                &mut changes,
                            );
                            baseline.windows.insert(key, new_window);
                        } else if let Some(old_window) = baseline.windows.remove(&key) {
                            diff_window_row(
                                &format!("windows.{key}"),
                                Some(&old_window),
                                None,
                                cause,
                                &mut changes,
                            );
                        }
                    }
                    None => {
                        baseline.surfaces.remove(&key);
                        if let Some(old_window) = baseline.windows.remove(&key) {
                            diff_window_row(
                                &format!("windows.{key}"),
                                Some(&old_window),
                                None,
                                cause,
                                &mut changes,
                            );
                        }
                    }
                }
            }
        } else if let Some(cause) = dirty_surfaces.values().next().copied() {
            state.observations.full_dirty.get_or_insert(cause);
        }
    }

    if let Some(cause) = state.observations.stack_dirty.take() {
        let next = project_stack(state);
        if baseline.stack != next {
            queue_prop_change(
                &mut changes,
                "stack".to_string(),
                PropValue::U64List(baseline.stack.clone()),
                PropValue::U64List(next.clone()),
                cause,
            );
            baseline.stack = next;
        }
    }
    let focus = project_focus(state);
    if baseline.focus != focus {
        let cause = state.observations.focus_cause;
        diff_focus("focus", &baseline.focus, &focus, cause, &mut changes);
        baseline.focus = focus;
    }
    state.observations.focus_cause = "wayland.focus";
    flush_prop_changes(state, changes);
    state.observations.watched_baseline = Some(baseline);
}

fn collect_snapshot_diff(
    old: &CompSnapshot,
    new: &CompSnapshot,
    cause: &'static str,
    pending: &mut PendingPropChanges,
) {
    let output_keys = old
        .outputs
        .keys()
        .chain(new.outputs.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    for key in output_keys {
        diff_output_row(
            &format!("outputs.{key}"),
            old.outputs.get(&key),
            new.outputs.get(&key),
            cause,
            pending,
        );
    }
    let surface_keys = old
        .surfaces
        .keys()
        .chain(new.surfaces.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    for key in surface_keys {
        diff_surface_row(
            &format!("surfaces.{key}"),
            old.surfaces.get(&key),
            new.surfaces.get(&key),
            cause,
            pending,
        );
    }
    let window_keys = old
        .windows
        .keys()
        .chain(new.windows.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    for key in window_keys {
        diff_window_row(
            &format!("windows.{key}"),
            old.windows.get(&key),
            new.windows.get(&key),
            cause,
            pending,
        );
    }
    queue_prop_change(
        pending,
        "stack".into(),
        PropValue::U64List(old.stack.clone()),
        PropValue::U64List(new.stack.clone()),
        cause,
    );
    diff_focus("focus", &old.focus, &new.focus, cause, pending);
    queue_prop_change(
        pending,
        "decoration.enabled".into(),
        PropValue::Bool(old.decoration.enabled),
        PropValue::Bool(new.decoration.enabled),
        cause,
    );
    queue_prop_change(
        pending,
        "decoration.style".into(),
        prop_str(old.decoration.style),
        prop_str(new.decoration.style),
        cause,
    );
    queue_prop_change(
        pending,
        "bindings.enabled".into(),
        PropValue::Bool(old.bindings.enabled),
        PropValue::Bool(new.bindings.enabled),
        cause,
    );
    queue_prop_change(
        pending,
        "bindings.profile".into(),
        prop_str(old.bindings.profile),
        prop_str(new.bindings.profile),
        cause,
    );
    queue_prop_change(
        pending,
        "bindings.table".into(),
        PropValue::BindingRows(old.bindings.table.clone()),
        PropValue::BindingRows(new.bindings.table.clone()),
        cause,
    );
    diff_corners(old, new, cause, pending);
}

fn diff_corners(
    old: &CompSnapshot,
    new: &CompSnapshot,
    cause: &'static str,
    pending: &mut PendingPropChanges,
) {
    let old = old.input.corners;
    let new = new.input.corners;
    for (leaf, old, new) in [
        (
            "enabled",
            PropValue::Bool(old.enabled),
            PropValue::Bool(new.enabled),
        ),
        (
            "deadzone_px",
            PropValue::F64(old.deadzone_px),
            PropValue::F64(new.deadzone_px),
        ),
        (
            "dwell_ms",
            PropValue::U64(old.dwell_ms),
            PropValue::U64(new.dwell_ms),
        ),
        (
            "velocity_max_px_s",
            PropValue::F64(old.velocity_max_px_s),
            PropValue::F64(new.velocity_max_px_s),
        ),
    ] {
        queue_prop_change(pending, format!("input.corners.{leaf}"), old, new, cause);
    }
}

fn diff_output_row(
    prefix: &str,
    old: Option<&OutputSnapshot>,
    new: Option<&OutputSnapshot>,
    cause: &'static str,
    pending: &mut PendingPropChanges,
) {
    match (old, new) {
        (None, None) => {}
        (None, Some(new)) => {
            queue_prop_change(
                pending,
                prefix.into(),
                PropValue::null(),
                PropValue::OutputRow(Box::new(new.clone())),
                cause,
            );
        }
        (Some(old), None) => {
            queue_prop_change(
                pending,
                prefix.into(),
                PropValue::OutputRow(Box::new(old.clone())),
                PropValue::null(),
                cause,
            );
        }
        (Some(old), Some(new)) => {
            for (leaf, old, new) in [
                ("name", prop_str(&old.name), prop_str(&new.name)),
                (
                    "default",
                    PropValue::Bool(old.default),
                    PropValue::Bool(new.default),
                ),
                ("x", PropValue::I32(old.x), PropValue::I32(new.x)),
                ("y", PropValue::I32(old.y), PropValue::I32(new.y)),
                (
                    "width",
                    PropValue::U32(old.width),
                    PropValue::U32(new.width),
                ),
                (
                    "height",
                    PropValue::U32(old.height),
                    PropValue::U32(new.height),
                ),
                (
                    "scale",
                    PropValue::F64(old.scale),
                    PropValue::F64(new.scale),
                ),
                (
                    "refresh_mhz",
                    PropValue::U32(old.refresh_mhz),
                    PropValue::U32(new.refresh_mhz),
                ),
                (
                    "usable.x",
                    PropValue::F32(old.usable.x),
                    PropValue::F32(new.usable.x),
                ),
                (
                    "usable.y",
                    PropValue::F32(old.usable.y),
                    PropValue::F32(new.usable.y),
                ),
                (
                    "usable.width",
                    PropValue::F32(old.usable.width),
                    PropValue::F32(new.usable.width),
                ),
                (
                    "usable.height",
                    PropValue::F32(old.usable.height),
                    PropValue::F32(new.usable.height),
                ),
            ] {
                queue_prop_change(pending, format!("{prefix}.{leaf}"), old, new, cause);
            }
        }
    }
}

fn diff_surface_row(
    prefix: &str,
    old: Option<&SurfaceSnapshot>,
    new: Option<&SurfaceSnapshot>,
    cause: &'static str,
    pending: &mut PendingPropChanges,
) {
    let (old, new) = match (old, new) {
        (None, None) => return,
        (None, Some(new)) => {
            queue_prop_change(
                pending,
                prefix.into(),
                PropValue::null(),
                PropValue::SurfaceRow(Box::new(new.clone())),
                cause,
            );
            return;
        }
        (Some(old), None) => {
            queue_prop_change(
                pending,
                prefix.into(),
                PropValue::SurfaceRow(Box::new(old.clone())),
                PropValue::null(),
                cause,
            );
            return;
        }
        (Some(old), Some(new)) => (old, new),
    };
    for (leaf, old, new) in [
        ("id", PropValue::U64(old.id), PropValue::U64(new.id)),
        ("role", prop_str(old.role), prop_str(new.role)),
        (
            "mapped",
            PropValue::Bool(old.mapped),
            PropValue::Bool(new.mapped),
        ),
        (
            "visible",
            PropValue::Bool(old.visible),
            PropValue::Bool(new.visible),
        ),
        ("x", PropValue::F32(old.x), PropValue::F32(new.x)),
        ("y", PropValue::F32(old.y), PropValue::F32(new.y)),
        (
            "width",
            PropValue::F32(old.width),
            PropValue::F32(new.width),
        ),
        (
            "height",
            PropValue::F32(old.height),
            PropValue::F32(new.height),
        ),
        ("band", prop_str(old.band), prop_str(new.band)),
        (
            "sequence",
            PropValue::U64(old.sequence),
            PropValue::U64(new.sequence),
        ),
        (
            "tree_index",
            PropValue::U32(old.tree_index),
            PropValue::U32(new.tree_index),
        ),
        ("parent", prop_opt_u64(old.parent), prop_opt_u64(new.parent)),
        (
            "output",
            prop_opt_string(old.output.as_deref()),
            prop_opt_string(new.output.as_deref()),
        ),
        (
            "title",
            prop_opt_string(old.title.as_deref()),
            prop_opt_string(new.title.as_deref()),
        ),
        (
            "app_id",
            prop_opt_string(old.app_id.as_deref()),
            prop_opt_string(new.app_id.as_deref()),
        ),
        (
            "focused",
            PropValue::Bool(old.focused),
            PropValue::Bool(new.focused),
        ),
        (
            "activated",
            PropValue::Bool(old.activated),
            PropValue::Bool(new.activated),
        ),
        (
            "maximized",
            PropValue::Bool(old.maximized),
            PropValue::Bool(new.maximized),
        ),
        (
            "minimized",
            PropValue::Bool(old.minimized),
            PropValue::Bool(new.minimized),
        ),
        (
            "decoration",
            prop_opt_string(old.decoration),
            prop_opt_string(new.decoration),
        ),
        (
            "foreign_id",
            prop_opt_string(old.foreign_id.as_deref()),
            prop_opt_string(new.foreign_id.as_deref()),
        ),
    ] {
        queue_prop_change(pending, format!("{prefix}.{leaf}"), old, new, cause);
    }
    diff_layer(
        prefix,
        old.layer.as_ref(),
        new.layer.as_ref(),
        cause,
        pending,
    );
}

fn diff_layer(
    prefix: &str,
    old: Option<&LayerSnapshot>,
    new: Option<&LayerSnapshot>,
    cause: &'static str,
    pending: &mut PendingPropChanges,
) {
    let old_stratum = old.map_or_else(PropValue::null, |row| prop_str(row.stratum));
    let new_stratum = new.map_or_else(PropValue::null, |row| prop_str(row.stratum));
    let old_interactivity = old.map_or_else(PropValue::null, |row| prop_str(row.interactivity));
    let new_interactivity = new.map_or_else(PropValue::null, |row| prop_str(row.interactivity));
    let old_zone = old.map_or_else(PropValue::null, |row| PropValue::I32(row.exclusive_zone));
    let new_zone = new.map_or_else(PropValue::null, |row| PropValue::I32(row.exclusive_zone));
    let old_binding = old.map_or_else(PropValue::null, |row| prop_str(row.binding));
    let new_binding = new.map_or_else(PropValue::null, |row| prop_str(row.binding));
    for (leaf, old, new) in [
        ("stratum", old_stratum, new_stratum),
        ("interactivity", old_interactivity, new_interactivity),
        ("exclusive_zone", old_zone, new_zone),
        ("binding", old_binding, new_binding),
    ] {
        queue_prop_change(pending, format!("{prefix}.layer.{leaf}"), old, new, cause);
    }
}

fn diff_window_row(
    prefix: &str,
    old: Option<&WindowSnapshot>,
    new: Option<&WindowSnapshot>,
    cause: &'static str,
    pending: &mut PendingPropChanges,
) {
    let (old, new) = match (old, new) {
        (None, None) => return,
        (None, Some(new)) => {
            queue_prop_change(
                pending,
                prefix.into(),
                PropValue::null(),
                PropValue::WindowRow(Box::new(new.clone())),
                cause,
            );
            return;
        }
        (Some(old), None) => {
            queue_prop_change(
                pending,
                prefix.into(),
                PropValue::WindowRow(Box::new(old.clone())),
                PropValue::null(),
                cause,
            );
            return;
        }
        (Some(old), Some(new)) => (old, new),
    };
    for (leaf, old, new) in [
        ("id", PropValue::U64(old.id), PropValue::U64(new.id)),
        (
            "foreign_id",
            prop_opt_string(old.foreign_id.as_deref()),
            prop_opt_string(new.foreign_id.as_deref()),
        ),
        (
            "title",
            prop_opt_string(old.title.as_deref()),
            prop_opt_string(new.title.as_deref()),
        ),
        (
            "app_id",
            prop_opt_string(old.app_id.as_deref()),
            prop_opt_string(new.app_id.as_deref()),
        ),
        ("x", PropValue::F32(old.x), PropValue::F32(new.x)),
        ("y", PropValue::F32(old.y), PropValue::F32(new.y)),
        (
            "width",
            PropValue::F32(old.width),
            PropValue::F32(new.width),
        ),
        (
            "height",
            PropValue::F32(old.height),
            PropValue::F32(new.height),
        ),
        (
            "focused",
            PropValue::Bool(old.focused),
            PropValue::Bool(new.focused),
        ),
        (
            "maximized",
            PropValue::Bool(old.maximized),
            PropValue::Bool(new.maximized),
        ),
        (
            "minimized",
            PropValue::Bool(old.minimized),
            PropValue::Bool(new.minimized),
        ),
        (
            "output",
            prop_opt_string(old.output.as_deref()),
            prop_opt_string(new.output.as_deref()),
        ),
    ] {
        queue_prop_change(pending, format!("{prefix}.{leaf}"), old, new, cause);
    }
}

fn diff_focus(
    prefix: &str,
    old: &FocusSnapshot,
    new: &FocusSnapshot,
    cause: &'static str,
    pending: &mut PendingPropChanges,
) {
    for (leaf, old, new) in [
        (
            "keyboard",
            prop_opt_u64(old.keyboard),
            prop_opt_u64(new.keyboard),
        ),
        (
            "exclusive_latch",
            prop_opt_u64(old.exclusive_latch),
            prop_opt_u64(new.exclusive_latch),
        ),
        (
            "pointer",
            prop_opt_u64(old.pointer),
            prop_opt_u64(new.pointer),
        ),
        (
            "pointer_grab",
            prop_str(old.pointer_grab),
            prop_str(new.pointer_grab),
        ),
        (
            "session_lock",
            prop_str(old.session_lock),
            prop_str(new.session_lock),
        ),
    ] {
        queue_prop_change(pending, format!("{prefix}.{leaf}"), old, new, cause);
    }
}

fn prop_str(value: &str) -> PropValue {
    PropValue::String(value.to_string())
}

fn prop_opt_string(value: Option<&str>) -> PropValue {
    value.map_or_else(PropValue::null, prop_str)
}

fn prop_opt_u64(value: Option<u64>) -> PropValue {
    value.map_or_else(PropValue::null, PropValue::U64)
}

fn queue_prop_change(
    pending: &mut PendingPropChanges,
    path: String,
    old: PropValue,
    new: PropValue,
    cause: &'static str,
) {
    if old == new || path.starts_with("port.") {
        return;
    }
    match pending.entry(path) {
        std::collections::btree_map::Entry::Vacant(entry) => {
            entry.insert((old, new, cause));
        }
        std::collections::btree_map::Entry::Occupied(mut entry) => {
            entry.get_mut().1 = new;
            if entry.get().0 == entry.get().1 {
                entry.remove();
            }
        }
    }
}

fn flush_prop_changes(state: &mut WaylandState, changes: PendingPropChanges) {
    for (path, (old, new, cause)) in changes {
        emit_prop_change(state, path, old, new, cause);
    }
}

fn emit_prop_change(
    state: &mut WaylandState,
    path: String,
    old: PropValue,
    new: PropValue,
    cause: &'static str,
) {
    if old == new || path.starts_with("port.") {
        return;
    }
    let unix_ms = unix_millis();
    state
        .observations
        .offer(|event_seq| ObservationRecord::PropsChanged {
            path,
            old,
            new,
            unix_ms,
            cause,
            event_seq,
        });
}

fn unix_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
        })
}

fn service_controls(state: &mut WaylandState) {
    let mut controls = std::mem::take(&mut state.pending_port_controls);
    if let Some(context) = state.port_context.as_ref() {
        for (active, order) in [
            (false, context.pending_idle_order.swap(0, Ordering::AcqRel)),
            (true, context.pending_active_order.swap(0, Ordering::AcqRel)),
        ] {
            if order != 0 {
                controls.push(PortControl::WatchState { active, order });
            }
        }
    }
    controls.sort_by_key(PortControl::order);
    let mut changes = PendingPropChanges::new();
    for control in &mut controls {
        if let PortControl::Set(request) = control {
            service_set(state, request, &mut changes);
        }
    }
    flush_set_changes(state, changes);

    let mut desired_active = state.observations.watched_baseline.is_some();
    let mut watches = Vec::new();
    for control in controls {
        match control {
            PortControl::Watch(request) => {
                desired_active = true;
                watches.push(request);
            }
            PortControl::WatchState { active, .. } => desired_active = active,
            PortControl::Set(_) => {}
        }
    }

    let needs_seed =
        !watches.is_empty() || (desired_active && state.observations.watched_baseline.is_none());
    let seed = needs_seed
        .then(|| {
            state
                .port_context
                .clone()
                .and_then(|context| snapshot(state, &context).map(|baseline| (context, baseline)))
        })
        .flatten();
    if desired_active {
        if let Some((_, baseline)) = &seed {
            state.observations.watched_baseline = Some(baseline.clone());
        }
    } else {
        state.observations.drop_watch();
    }
    for request in watches {
        let reply = seed
            .as_ref()
            .map_or(ControlReply::Busy, |(context, _)| ControlReply::Watch {
                topic: topic_name(&context.service, PROPS_TOPIC_SUFFIX),
                event_seq: state.observations.event_seq,
                lost_count: context.lost_count.load(Ordering::Acquire),
            });
        let _ = request.reply.send(reply);
    }
}

fn service_set(
    state: &mut WaylandState,
    request: &mut PortSetRequest,
    changes: &mut PendingPropChanges,
) {
    let path = request.path.clone();
    let old_config = state.observations.corner_config;
    let mut new_config = old_config;
    let validated = match validate_corner_value(&path, &request.value) {
        Ok(value) => value,
        Err(error) => {
            if let Some(reply) = request.reply.take() {
                let _ = reply.send(ControlReply::Validation(error));
            }
            return;
        }
    };
    let (old, new) = apply_corner_value(&mut new_config, validated);
    if old != new {
        state.apply_corner_config(new_config);
        queue_prop_change(changes, path.clone(), old.clone(), new.clone(), "props.set");
    }
    if let Some(reply) = request.reply.take() {
        let _ = reply.send(ControlReply::Set { path, old, new });
    }
}

fn flush_set_changes(state: &mut WaylandState, changes: PendingPropChanges) {
    flush_prop_changes(state, changes);
    if let Some(baseline) = state.observations.watched_baseline.as_mut() {
        baseline.input.corners = state.observations.corner_config.into();
    }
}

pub(crate) fn validate_corner_value(
    path: &str,
    value: &Value,
) -> Result<ValidatedCornerValue, SetValidationError> {
    match path {
        "input.corners.enabled" => {
            let Some(value) = value.as_bool() else {
                return Err(invalid_value(path, "bool", "true|false"));
            };
            Ok(ValidatedCornerValue::Enabled(value))
        }
        "input.corners.deadzone_px" => {
            let value = finite_number(path, value, "finite number", "1.0..=256.0")?;
            if !(1.0..=256.0).contains(&value) {
                return Err(invalid_value(path, "finite number", "1.0..=256.0"));
            }
            Ok(ValidatedCornerValue::DeadzonePx(value))
        }
        "input.corners.dwell_ms" => {
            let Some(value) = value.as_u64().filter(|value| *value <= 5_000) else {
                return Err(invalid_value(path, "integer", "0..=5000"));
            };
            Ok(ValidatedCornerValue::DwellMs(value))
        }
        "input.corners.velocity_max_px_s" => {
            let value = finite_number(path, value, "finite number", "1.0..=20000.0")?;
            if !(1.0..=20_000.0).contains(&value) {
                return Err(invalid_value(path, "finite number", "1.0..=20000.0"));
            }
            Ok(ValidatedCornerValue::VelocityMaxPxS(value))
        }
        _ if path.starts_with("input.corners.") => Err(SetValidationError::UnknownPath),
        _ if known_read_only_path(path) => Err(SetValidationError::ReadOnly),
        _ => Err(SetValidationError::UnknownPath),
    }
}

fn apply_corner_value(
    config: &mut CornerConfig,
    value: ValidatedCornerValue,
) -> (PropValue, PropValue) {
    match value {
        ValidatedCornerValue::Enabled(value) => {
            let old = config.enabled;
            config.enabled = value;
            (PropValue::Bool(old), PropValue::Bool(value))
        }
        ValidatedCornerValue::DeadzonePx(value) => {
            let old = config.deadzone_px;
            config.deadzone_px = value;
            (PropValue::F64(old), PropValue::F64(value))
        }
        ValidatedCornerValue::DwellMs(value) => {
            let old = config.dwell_ms;
            config.dwell_ms = value;
            (PropValue::U64(old), PropValue::U64(value))
        }
        ValidatedCornerValue::VelocityMaxPxS(value) => {
            let old = config.velocity_max_px_s;
            config.velocity_max_px_s = value;
            (PropValue::F64(old), PropValue::F64(value))
        }
    }
}

fn finite_number(
    path: &str,
    value: &Value,
    expected: &'static str,
    range: &'static str,
) -> Result<f64, SetValidationError> {
    value
        .as_f64()
        .filter(|number| number.is_finite())
        .ok_or_else(|| invalid_value(path, expected, range))
}

fn invalid_value(path: &str, expected: &'static str, range: &'static str) -> SetValidationError {
    SetValidationError::InvalidValue {
        path: path.to_string(),
        expected,
        range,
    }
}

fn known_read_only_path(path: &str) -> bool {
    const ROOTS: &[&str] = &[
        "info",
        "outputs",
        "surfaces",
        "windows",
        "stack",
        "focus",
        "decoration",
        "bindings",
        "port",
    ];
    path == "input"
        || path == "input.corners"
        || ROOTS
            .iter()
            .any(|root| path == *root || path.starts_with(&format!("{root}.")))
}

#[cfg(test)]
mod tests {
    use std::{
        alloc::{GlobalAlloc, Layout, System},
        cell::Cell,
    };

    use super::*;

    thread_local! {
        static TRACKED_ALLOCATIONS: Cell<Option<usize>> = const { Cell::new(None) };
    }

    struct TestAllocator;

    #[global_allocator]
    static TEST_ALLOCATOR: TestAllocator = TestAllocator;

    unsafe impl GlobalAlloc for TestAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let _ = TRACKED_ALLOCATIONS.try_with(|count| {
                if let Some(current) = count.get() {
                    count.set(Some(current.saturating_add(1)));
                }
            });
            // SAFETY: this wrapper preserves System's allocation contract.
            unsafe { System.alloc(layout) }
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            let _ = TRACKED_ALLOCATIONS.try_with(|count| {
                if let Some(current) = count.get() {
                    count.set(Some(current.saturating_add(1)));
                }
            });
            // SAFETY: this wrapper preserves System's allocation contract.
            unsafe { System.alloc_zeroed(layout) }
        }

        unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
            // SAFETY: pointer/layout came from this System-backed allocator.
            unsafe { System.dealloc(pointer, layout) }
        }

        unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
            let _ = TRACKED_ALLOCATIONS.try_with(|count| {
                if let Some(current) = count.get() {
                    count.set(Some(current.saturating_add(1)));
                }
            });
            // SAFETY: pointer/layout came from System and size is forwarded.
            unsafe { System.realloc(pointer, layout, size) }
        }
    }

    fn allocations_during(run: impl FnOnce()) -> usize {
        TRACKED_ALLOCATIONS.with(|count| {
            assert_eq!(count.replace(Some(0)), None);
        });
        run();
        TRACKED_ALLOCATIONS.with(|count| count.replace(None).expect("tracking was armed"))
    }

    fn merged_markers(outbox: &ObservationOutbox) -> LossInterval {
        let mut marker = outbox.markers.recv().expect("at least one loss marker");
        while let Ok(next) = outbox.markers.try_recv() {
            marker.merge(next);
        }
        marker
    }

    #[test]
    fn bounded_marker_lane_merges_exact_loss_without_dropping_a_marker() {
        let lost = Arc::new(AtomicU64::new(0));
        let (mut producer, outbox) = test_outbox(Arc::clone(&lost), 2);
        assert_eq!(outbox.records.capacity(), Some(2));
        assert_eq!(outbox.markers.capacity(), Some(2));
        for sequence in 1..=6 {
            if sequence % 2 == 0 {
                producer.offer(ObservationRecord::FocusChanged {
                    keyboard: Some(sequence),
                    previous: None,
                    exclusive_latch: None,
                    event_seq: sequence,
                });
            } else {
                producer.offer(ObservationRecord::PropsChanged {
                    path: "input.corners.enabled".into(),
                    old: PropValue::Bool(true),
                    new: PropValue::Bool(false),
                    unix_ms: 0,
                    cause: "props.set",
                    event_seq: sequence,
                });
            }
        }
        assert_eq!(lost.load(Ordering::Acquire), 4);
        let interval = merged_markers(&outbox);
        assert_eq!((interval.first_lost_seq, interval.last_lost_seq), (1, 4));
        assert_eq!(
            interval.topics.iter().collect::<Vec<_>>(),
            [PROPS_TOPIC_SUFFIX, FOCUS_TOPIC_SUFFIX]
        );
        assert_eq!(interval.cause, LossCause::OutboxOverflow);
        assert_eq!(
            outbox
                .records
                .try_iter()
                .map(|record| record.event_seq())
                .collect::<Vec<_>>(),
            [5, 6]
        );
        assert!(
            outbox.markers.is_empty(),
            "the marker was merged, not dropped"
        );
    }

    #[test]
    fn marker_normalisation_preserves_the_held_survivor_boundary() {
        let lost = Arc::new(AtomicU64::new(0));
        let (mut producer, outbox) = test_outbox(Arc::clone(&lost), 2);
        for event_seq in 1..=3 {
            producer.offer(ObservationRecord::FocusChanged {
                keyboard: Some(event_seq),
                previous: None,
                exclusive_latch: None,
                event_seq,
            });
        }
        let held = outbox.records.recv().expect("oldest survivor is held");
        assert_eq!(held.event_seq(), 2);
        outbox.held_record_seq.store(2, Ordering::Release);
        for event_seq in 4..=6 {
            producer.offer(ObservationRecord::FocusChanged {
                keyboard: Some(event_seq),
                previous: None,
                exclusive_latch: None,
                event_seq,
            });
        }

        let before = outbox.markers.recv().expect("marker before survivor");
        let after = outbox.markers.recv().expect("marker after survivor");
        assert_eq!((before.first_lost_seq, before.last_lost_seq), (1, 1));
        assert_eq!((after.first_lost_seq, after.last_lost_seq), (3, 4));
        assert_eq!(lost.load(Ordering::Acquire), 3);
    }

    #[test]
    fn loss_interval_is_closed_when_its_marker_leaves_the_channel() {
        let lost = Arc::new(AtomicU64::new(0));
        let (mut producer, outbox) = test_outbox(Arc::clone(&lost), 3);
        for sequence in 1..=4 {
            producer.offer(ObservationRecord::FocusChanged {
                keyboard: Some(sequence),
                previous: None,
                exclusive_latch: None,
                event_seq: sequence,
            });
        }
        let first = outbox.markers.recv().unwrap();
        assert_eq!((first.first_lost_seq, first.last_lost_seq), (1, 1));

        producer.offer(ObservationRecord::PropsChanged {
            path: "input.corners.enabled".into(),
            old: PropValue::Bool(true),
            new: PropValue::Bool(false),
            unix_ms: 0,
            cause: "props.set",
            event_seq: 5,
        });
        producer.offer(ObservationRecord::FocusChanged {
            keyboard: Some(6),
            previous: None,
            exclusive_latch: None,
            event_seq: 6,
        });

        let second = merged_markers(&outbox);
        assert_eq!((second.first_lost_seq, second.last_lost_seq), (2, 3));
        assert_eq!(
            second.topics.iter().collect::<Vec<_>>(),
            [FOCUS_TOPIC_SUFFIX]
        );
        assert_eq!(
            lost.load(Ordering::Acquire),
            3,
            "the later interval cannot be absorbed into the earlier watermark"
        );
    }

    #[test]
    fn publisher_loss_dominates_interval_merges_in_both_orders() {
        let record = ObservationRecord::FocusChanged {
            keyboard: Some(1),
            previous: None,
            exclusive_latch: None,
            event_seq: 1,
        };
        let overflow = LossInterval::from_record(&record, LossCause::OutboxOverflow);
        let publisher = LossInterval::from_record(&record, LossCause::PublisherLoss);

        let mut overflow_then_publisher = overflow;
        overflow_then_publisher.merge(publisher);
        let mut publisher_then_overflow = publisher;
        publisher_then_overflow.merge(overflow);
        assert_eq!(overflow_then_publisher.cause, LossCause::PublisherLoss);
        assert_eq!(publisher_then_overflow.cause, LossCause::PublisherLoss);
    }

    #[test]
    fn repeated_capacity_two_overflow_allocates_nothing_in_offer() {
        let lost = Arc::new(AtomicU64::new(0));
        let (mut producer, outbox) = test_outbox(Arc::clone(&lost), 2);
        let allocations = allocations_during(|| {
            for event_seq in 1..=1_024 {
                producer.offer(ObservationRecord::FocusChanged {
                    keyboard: Some(event_seq),
                    previous: None,
                    exclusive_latch: None,
                    event_seq,
                });
            }
        });

        assert_eq!(allocations, 0, "offer must remain allocation-free");
        assert_eq!(lost.load(Ordering::Acquire), 1_022);
        let generation = outbox.marker_update_generation.load(Ordering::Acquire);
        assert!(
            generation > 0,
            "repeated overflow normalised the marker lane"
        );
        assert!(
            generation.is_multiple_of(2),
            "marker normalisation finished stable"
        );
        let marker = merged_markers(&outbox);
        assert_eq!((marker.first_lost_seq, marker.last_lost_seq), (1, 1_022));
        assert_eq!(
            outbox
                .records
                .try_iter()
                .map(|record| record.event_seq())
                .collect::<Vec<_>>(),
            [1_023, 1_024]
        );
    }

    #[test]
    fn keyed_row_add_is_one_row_granular_change() {
        let row = OutputSnapshot {
            name: "nested".into(),
            default: true,
            x: 0,
            y: 0,
            width: 640,
            height: 480,
            scale: 1.0,
            refresh_mhz: 60_000,
            usable: crate::protocol::port_snapshot::RectSnapshot {
                x: 0.0,
                y: 0.0,
                width: 640.0,
                height: 480.0,
            },
        };
        let mut changes = PendingPropChanges::new();
        diff_output_row(
            "outputs.o_nested",
            None,
            Some(&row),
            "output.geometry",
            &mut changes,
        );
        assert_eq!(changes.len(), 1);
        assert_eq!(
            changes.remove("outputs.o_nested"),
            Some((
                PropValue::null(),
                PropValue::OutputRow(Box::new(row.clone())),
                "output.geometry"
            ))
        );
        let wire = ObservationRecord::PropsChanged {
            path: "outputs.o_nested".into(),
            old: PropValue::null(),
            new: PropValue::OutputRow(Box::new(row)),
            unix_ms: 0,
            cause: "output.geometry",
            event_seq: 1,
        }
        .wire();
        let body = serde_json::from_str::<Value>(&wire.body).expect("row frame body");
        assert!(body["old"].is_null());
        assert_eq!(body["path"], "outputs.o_nested");
        assert_eq!(body["new"]["width"], 640);

        let surface = SurfaceSnapshot {
            id: 7,
            role: "toplevel",
            mapped: true,
            visible: true,
            x: 1.0,
            y: 2.0,
            width: 3.0,
            height: 4.0,
            band: "normal",
            sequence: 1,
            tree_index: 0,
            parent: None,
            output: Some("o_nested".into()),
            title: Some(Arc::from("row")),
            app_id: None,
            focused: false,
            activated: false,
            maximized: false,
            minimized: false,
            decoration: Some("server"),
            layer: None,
            foreign_id: Some("f_7".into()),
        };
        let window = project_window_row(&surface);
        let mut keyed = PendingPropChanges::new();
        diff_surface_row(
            "surfaces.s7",
            None,
            Some(&surface),
            "wayland.map",
            &mut keyed,
        );
        diff_window_row("windows.s7", None, Some(&window), "wayland.map", &mut keyed);
        assert_eq!(
            keyed.keys().cloned().collect::<Vec<_>>(),
            ["surfaces.s7", "windows.s7"]
        );
        let mut removed = PendingPropChanges::new();
        diff_surface_row(
            "surfaces.s7",
            Some(&surface),
            None,
            "wayland.unmap",
            &mut removed,
        );
        assert!(matches!(
            removed.get("surfaces.s7"),
            Some((
                PropValue::SurfaceRow(_),
                PropValue::Null(()),
                "wayland.unmap"
            ))
        ));
    }

    #[test]
    fn property_reducer_coalesces_each_path_and_excludes_operational_leaves() {
        let mut pending = PendingPropChanges::new();
        queue_prop_change(
            &mut pending,
            "surfaces.s2.title".into(),
            prop_str("old"),
            prop_str("middle"),
            "wayland.map",
        );
        queue_prop_change(
            &mut pending,
            "surfaces.s2.title".into(),
            prop_str("middle"),
            prop_str("new"),
            "wayland.focus",
        );
        queue_prop_change(
            &mut pending,
            "port.event_seq".into(),
            PropValue::U64(1),
            PropValue::U64(2),
            "wayland.map",
        );
        assert_eq!(
            pending.remove("surfaces.s2.title"),
            Some((prop_str("old"), prop_str("new"), "wayland.map"))
        );
        assert!(pending.is_empty());

        queue_prop_change(
            &mut pending,
            "focus.keyboard".into(),
            PropValue::null(),
            PropValue::U64(2),
            "wayland.focus",
        );
        queue_prop_change(
            &mut pending,
            "focus.keyboard".into(),
            PropValue::U64(2),
            PropValue::null(),
            "wayland.focus",
        );
        assert!(pending.is_empty());
    }

    #[test]
    fn exact_topic_commands_and_flat_bodies() {
        let record = ObservationRecord::SurfaceMapped {
            id: 7,
            role: "toplevel".into(),
            foreign_id: Some("f_7".into()),
            event_seq: 9,
        };
        let wire = record.wire();
        assert_eq!(record.topic_suffix(), SURFACE_MAPPED_TOPIC_SUFFIX);
        assert_eq!(wire.get("command"), Some(SURFACE_MAPPED_TOPIC_SUFFIX));
        assert_eq!(
            topic_name("comp-nested", record.topic_suffix()),
            "comp-nested.surface.mapped"
        );
        assert_eq!(
            serde_json::from_str::<Value>(&wire.body).unwrap(),
            json!({
                "id": 7,
                "role": "toplevel",
                "foreign_id": "f_7",
                "event_seq": 9,
            })
        );
    }

    #[test]
    fn every_topic_uses_the_registered_service_and_an_unprefixed_command() {
        let records = [
            ObservationRecord::PropsChanged {
                path: "input.corners.dwell_ms".into(),
                old: PropValue::U64(200),
                new: PropValue::U64(250),
                unix_ms: 0,
                cause: "props.set",
                event_seq: 1,
            },
            ObservationRecord::SurfaceMapped {
                id: 1,
                role: "toplevel".into(),
                foreign_id: None,
                event_seq: 2,
            },
            ObservationRecord::SurfaceUnmapped {
                id: 1,
                role: "toplevel".into(),
                foreign_id: None,
                event_seq: 3,
            },
            ObservationRecord::FocusChanged {
                keyboard: Some(1),
                previous: None,
                exclusive_latch: None,
                event_seq: 4,
            },
            ObservationRecord::OutputChanged {
                output: "o_nested".into(),
                row: OutputSnapshot {
                    name: "nested".into(),
                    default: true,
                    x: 0,
                    y: 0,
                    width: 1,
                    height: 1,
                    scale: 1.0,
                    refresh_mhz: 60_000,
                    usable: crate::protocol::port_snapshot::RectSnapshot {
                        x: 0.0,
                        y: 0.0,
                        width: 1.0,
                        height: 1.0,
                    },
                },
                event_seq: 5,
            },
            ObservationRecord::CornerEntered {
                output: "o_nested".into(),
                corner: Corner::TopLeft,
                dwell_ms: 200,
                event_seq: 6,
            },
            ObservationRecord::CornerLeft {
                output: "o_nested".into(),
                corner: Corner::TopLeft,
                dwell_ms: 200,
                event_seq: 7,
            },
        ];
        let suffixes = [
            "props.changed",
            "surface.mapped",
            "surface.unmapped",
            "focus.changed",
            "output.changed",
            "corner.entered",
            "corner.left",
        ];
        for (record, suffix) in records.iter().zip(suffixes) {
            assert_eq!(record.topic_suffix(), suffix);
            assert_eq!(record.wire().get("command"), Some(suffix));
            assert_eq!(
                topic_name("observer-test", suffix),
                format!("observer-test.{suffix}")
            );
        }
    }

    #[test]
    fn corner_property_validation_accepts_endpoints_and_rejects_wrong_json_types() {
        for (path, value) in [
            ("input.corners.enabled", json!(false)),
            ("input.corners.deadzone_px", json!(1.0)),
            ("input.corners.deadzone_px", json!(256.0)),
            ("input.corners.dwell_ms", json!(0)),
            ("input.corners.dwell_ms", json!(5_000)),
            ("input.corners.velocity_max_px_s", json!(1.0)),
            ("input.corners.velocity_max_px_s", json!(20_000.0)),
        ] {
            assert!(
                validate_corner_value(path, &value).is_ok(),
                "{path}={value}"
            );
        }
        for (path, value) in [
            ("input.corners.enabled", json!(1)),
            ("input.corners.deadzone_px", json!("12")),
            ("input.corners.dwell_ms", json!(1.5)),
            ("input.corners.dwell_ms", json!(-1)),
            ("input.corners.velocity_max_px_s", json!(null)),
        ] {
            assert!(matches!(
                validate_corner_value(path, &value),
                Err(SetValidationError::InvalidValue { .. })
            ));
        }
    }

    #[test]
    fn event_sequence_exhaustion_offers_max_once_then_stops() {
        let event_loop = smithay::reexports::calloop::EventLoop::<WaylandState>::try_new()
            .expect("test event loop");
        let lost = Arc::new(AtomicU64::new(0));
        let (producer, outbox) = outbox(lost);
        let watermark = Arc::new(AtomicU64::new(0));
        let mut state =
            ObservationState::new(producer, Arc::clone(&watermark), event_loop.handle());
        state.event_seq = u64::MAX - 1;
        state.offer(|event_seq| ObservationRecord::FocusChanged {
            keyboard: None,
            previous: None,
            exclusive_latch: None,
            event_seq,
        });
        state.offer(|event_seq| ObservationRecord::FocusChanged {
            keyboard: None,
            previous: None,
            exclusive_latch: None,
            event_seq,
        });
        assert_eq!(
            outbox
                .records
                .try_iter()
                .map(|record| record.event_seq())
                .collect::<Vec<_>>(),
            [u64::MAX]
        );
        assert_eq!(watermark.load(Ordering::Acquire), u64::MAX);
        assert!(state.event_seq_exhausted);
    }
}
