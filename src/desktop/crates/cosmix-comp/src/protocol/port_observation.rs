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
use serde_json::{Value, json};
use smithay::output::Output;
use smithay::reexports::calloop::{
    LoopHandle, RegistrationToken,
    timer::{TimeoutAction, Timer},
};
use smithay::reexports::wayland_server::Resource;

use crate::port::{PortControl, PortSetRequest};

use super::{
    SurfaceId, WaylandState,
    corner::{Corner, CornerConfig, CornerDetector, CornerEvent},
    port_snapshot::{
        CompSnapshot, FocusSnapshot, OutputSnapshot, WindowSnapshot, error, project_focus,
        project_outputs, project_stack, project_surface_by_id, project_window_row, snapshot,
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

type PendingPropChanges = BTreeMap<String, (Value, Value, &'static str)>;

#[cfg(test)]
const OUTBOX_CAPACITY: usize = 2;
#[cfg(not(test))]
const OUTBOX_CAPACITY: usize = 64;

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ObservationRecord {
    PropsChanged {
        path: String,
        old: Value,
        new: Value,
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
                    "old": old,
                    "new": new,
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

fn rfc3339_millis(unix_ms: i64) -> String {
    Utc.timestamp_millis_opt(unix_ms)
        .single()
        .unwrap_or_else(Utc::now)
        .to_rfc3339_opts(SecondsFormat::Millis, true)
}

#[derive(Clone)]
pub(crate) struct ObservationProducer {
    sender: Sender<ObservationRecord>,
    eviction: Receiver<ObservationRecord>,
    lost_count: Arc<AtomicU64>,
}

impl ObservationProducer {
    pub(crate) fn offer(&self, mut record: ObservationRecord) {
        loop {
            match self.sender.try_send(record) {
                Ok(()) => return,
                Err(TrySendError::Disconnected(_)) => {
                    self.lost_count.fetch_add(1, Ordering::AcqRel);
                    return;
                }
                Err(TrySendError::Full(returned)) => {
                    record = returned;
                    match self.eviction.try_recv() {
                        Ok(_) => {
                            self.lost_count.fetch_add(1, Ordering::AcqRel);
                        }
                        Err(TryRecvError::Empty) => continue,
                        Err(TryRecvError::Disconnected) => {
                            self.lost_count.fetch_add(1, Ordering::AcqRel);
                            return;
                        }
                    }
                }
            }
        }
    }
}

pub(crate) fn outbox(
    lost_count: Arc<AtomicU64>,
) -> (ObservationProducer, Receiver<ObservationRecord>) {
    let (sender, receiver) = crossbeam_channel::bounded(OUTBOX_CAPACITY);
    (
        ObservationProducer {
            sender,
            eviction: receiver.clone(),
            lost_count,
        },
        receiver,
    )
}

#[derive(Clone, Debug)]
struct SurfaceEdgeStart {
    mapped: bool,
    role: String,
    foreign_id: Option<String>,
}

#[derive(Clone, Debug)]
struct OutputEdgeStart {
    row: OutputSnapshot,
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
    property_dirty_outputs: BTreeMap<String, &'static str>,
    pending_focus: Option<FocusSnapshot>,
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
            event_seq_watermark,
        }
    }

    fn next_seq(&mut self) -> u64 {
        self.event_seq = self.event_seq.saturating_add(1);
        self.event_seq_watermark
            .store(self.event_seq, Ordering::Release);
        self.event_seq
    }

    fn offer(&mut self, build: impl FnOnce(u64) -> ObservationRecord) -> u64 {
        let sequence = self.next_seq();
        self.producer.offer(build(sequence));
        sequence
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
            self.observations.pending_focus = Some(project_focus(self));
            self.observations.focus_cause = cause;
        }
    }

    pub(crate) fn mark_output_before_change(&mut self, output: &Output, cause: &'static str) {
        let Some(projection) = project_outputs(self) else {
            return;
        };
        let Some((_, key)) = projection
            .keys
            .iter()
            .find(|(candidate, _)| candidate == output)
        else {
            return;
        };
        if let Some(row) = projection.rows.get(key) {
            self.observations
                .property_dirty_outputs
                .entry(key.clone())
                .or_insert(cause);
            self.observations
                .dirty_outputs
                .entry(key.clone())
                .or_insert_with(|| OutputEdgeStart { row: row.clone() });
        }
    }

    #[cfg(any(all(feature = "kms-live", not(test)), test))]
    pub(crate) fn mark_all_outputs_before_change(&mut self, cause: &'static str) {
        let Some(projection) = project_outputs(self) else {
            return;
        };
        for (key, row) in projection.rows {
            self.observations
                .property_dirty_outputs
                .entry(key.clone())
                .or_insert(cause);
            self.observations
                .dirty_outputs
                .entry(key)
                .or_insert(OutputEdgeStart { row });
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
            self.observations.corner_output_keys.clear();
            self.reset_corner_detector();
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
}

pub(super) fn service_observations(state: &mut WaylandState) {
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
    let current = project_focus(state);
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

fn service_output_edges(state: &mut WaylandState) {
    let pending = std::mem::take(&mut state.observations.dirty_outputs);
    let topology_dirty = std::mem::take(&mut state.observations.output_topology_dirty);
    if pending.is_empty() && !topology_dirty {
        return;
    }
    let final_rows = project_outputs(state)
        .map(|projection| projection.rows)
        .unwrap_or_default();
    let old_keys = pending.keys().cloned().collect::<BTreeSet<_>>();
    let final_keys = final_rows.keys().cloned().collect::<BTreeSet<_>>();
    let topology_replaced = topology_dirty && old_keys != final_keys;
    if topology_replaced {
        state.reset_corner_detector();
    }
    let mut emitted = BTreeSet::new();
    for (key, old) in pending {
        let Some(row) = final_rows.get(&key) else {
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
        let row = row.clone();
        state
            .observations
            .offer(|event_seq| ObservationRecord::OutputChanged {
                output: key.clone(),
                row,
                event_seq,
            });
        emitted.insert(key);
    }
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

    let output_projection = project_outputs(state);
    let dirty_outputs = std::mem::take(&mut state.observations.property_dirty_outputs);
    if let Some(projection) = &output_projection {
        for (key, cause) in dirty_outputs {
            let old = baseline.outputs.get(&key).cloned();
            let new = projection.rows.get(&key).cloned();
            diff_row(
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
    }
    let dirty_surfaces = std::mem::take(&mut state.observations.dirty_surfaces);
    if let Some(projection) = &output_projection {
        for (raw_id, cause) in dirty_surfaces {
            let key = format!("s{raw_id}");
            let old_surface = baseline.surfaces.get(&key).cloned();
            let new_surface = project_surface_by_id(state, SurfaceId(raw_id), &projection.keys);
            diff_row(
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
                        diff_row(
                            &format!("windows.{key}"),
                            old_window.as_ref(),
                            Some(&new_window),
                            cause,
                            &mut changes,
                        );
                        baseline.windows.insert(key, new_window);
                    } else if let Some(old_window) = baseline.windows.remove(&key) {
                        diff_row::<WindowSnapshot>(
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
                        diff_row::<WindowSnapshot>(
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
    }

    if let Some(cause) = state.observations.stack_dirty.take() {
        let next = project_stack(state);
        if baseline.stack != next {
            queue_prop_change(
                &mut changes,
                "stack".to_string(),
                json!(baseline.stack),
                json!(next),
                cause,
            );
            baseline.stack = next;
        }
    }
    let focus = project_focus(state);
    if baseline.focus != focus {
        let cause = state.observations.focus_cause;
        diff_row(
            "focus",
            Some(&baseline.focus),
            Some(&focus),
            cause,
            &mut changes,
        );
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
    let old = serde_json::to_value(old).unwrap_or(Value::Null);
    let new = serde_json::to_value(new).unwrap_or(Value::Null);
    let mut changes = BTreeMap::new();
    collect_leaf_diffs("", &old, &new, &mut changes);
    for (path, (old, new)) in changes {
        queue_prop_change(pending, path, old, new, cause);
    }
}

fn diff_row<T: serde::Serialize>(
    prefix: &str,
    old: Option<&T>,
    new: Option<&T>,
    cause: &'static str,
    pending: &mut PendingPropChanges,
) {
    let old = old
        .and_then(|row| serde_json::to_value(row).ok())
        .unwrap_or(Value::Null);
    let new = new
        .and_then(|row| serde_json::to_value(row).ok())
        .unwrap_or(Value::Null);
    let mut changes = BTreeMap::new();
    collect_leaf_diffs(prefix, &old, &new, &mut changes);
    for (path, (old, new)) in changes {
        queue_prop_change(pending, path, old, new, cause);
    }
}

fn queue_prop_change(
    pending: &mut PendingPropChanges,
    path: String,
    old: Value,
    new: Value,
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

fn collect_leaf_diffs(
    prefix: &str,
    old: &Value,
    new: &Value,
    changes: &mut BTreeMap<String, (Value, Value)>,
) {
    if old == new {
        return;
    }
    let old_object = old.as_object();
    let new_object = new.as_object();
    if old_object.is_none() && new_object.is_none() {
        changes.insert(prefix.to_string(), (old.clone(), new.clone()));
        return;
    }
    let mut keys = BTreeSet::new();
    if let Some(object) = old_object {
        keys.extend(object.keys().cloned());
    }
    if let Some(object) = new_object {
        keys.extend(object.keys().cloned());
    }
    for key in keys {
        let child_prefix = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        collect_leaf_diffs(
            &child_prefix,
            old_object
                .and_then(|object| object.get(&key))
                .unwrap_or(&Value::Null),
            new_object
                .and_then(|object| object.get(&key))
                .unwrap_or(&Value::Null),
            changes,
        );
    }
}

fn emit_prop_change(
    state: &mut WaylandState,
    path: String,
    old: Value,
    new: Value,
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
    if let Some(order) = state
        .port_context
        .as_ref()
        .map(|context| context.pending_idle_order.swap(0, Ordering::AcqRel))
        .filter(|order| *order != 0)
    {
        controls.push(PortControl::WatchState {
            active: false,
            order,
        });
    }
    controls.sort_by_key(PortControl::order);
    let mut changes = PendingPropChanges::new();
    let mut watch_seed = None;
    for control in controls {
        match control {
            PortControl::Watch(request) => {
                flush_set_changes(state, std::mem::take(&mut changes));
                let reply = state.port_context.clone().and_then(|context| {
                    if watch_seed.is_none() {
                        watch_seed = snapshot(state, &context);
                    }
                    watch_seed.clone().map(|baseline| {
                        state.observations.watched_baseline = Some(baseline);
                        (
                            0,
                            Arc::from(
                                json!({
                                    "topic": topic_name(
                                        &context.service,
                                        PROPS_TOPIC_SUFFIX,
                                    ),
                                    "event_seq": state.observations.event_seq,
                                    "lost_count": context.lost_count.load(Ordering::Acquire),
                                })
                                .to_string(),
                            ),
                        )
                    })
                });
                let _ = request.reply.send(reply.unwrap_or_else(|| error("busy")));
            }
            PortControl::Set(request) => {
                watch_seed = None;
                service_set(state, request, &mut changes);
            }
            PortControl::WatchState { active, .. } => {
                if !active {
                    flush_set_changes(state, std::mem::take(&mut changes));
                    watch_seed = None;
                    state.observations.drop_watch();
                }
            }
        }
    }
    flush_set_changes(state, changes);
}

fn service_set(
    state: &mut WaylandState,
    request: PortSetRequest,
    changes: &mut PendingPropChanges,
) {
    let path = request.path;
    let old_config = state.observations.corner_config;
    let mut new_config = old_config;
    let (old, new) = match validate_corner_value(&path, &request.value, &mut new_config) {
        Ok(values) => values,
        Err(reply) => {
            let _ = request.reply.send(reply);
            return;
        }
    };
    if old != new {
        state.apply_corner_config(new_config);
        queue_prop_change(changes, path.clone(), old.clone(), new.clone(), "props.set");
    }
    let _ = request.reply.send((
        0,
        Arc::from(json!({"path": path, "old": old, "new": new}).to_string()),
    ));
}

fn flush_set_changes(state: &mut WaylandState, changes: PendingPropChanges) {
    flush_prop_changes(state, changes);
    if let Some(baseline) = state.observations.watched_baseline.as_mut() {
        baseline.input.corners = state.observations.corner_config.into();
    }
}

fn validate_corner_value(
    path: &str,
    value: &Value,
    config: &mut CornerConfig,
) -> Result<(Value, Value), (u8, Arc<str>)> {
    match path {
        "input.corners.enabled" => {
            let Some(value) = value.as_bool() else {
                return Err(invalid_value(path, "bool", "true|false"));
            };
            let old = config.enabled;
            config.enabled = value;
            Ok((json!(old), json!(value)))
        }
        "input.corners.deadzone_px" => {
            let value = finite_number(path, value, "finite number", "1.0..=256.0")?;
            if !(1.0..=256.0).contains(&value) {
                return Err(invalid_value(path, "finite number", "1.0..=256.0"));
            }
            let old = config.deadzone_px;
            config.deadzone_px = value;
            Ok((json!(old), json!(value)))
        }
        "input.corners.dwell_ms" => {
            let Some(value) = value.as_u64().filter(|value| *value <= 5_000) else {
                return Err(invalid_value(path, "integer", "0..=5000"));
            };
            let old = config.dwell_ms;
            config.dwell_ms = value;
            Ok((json!(old), json!(value)))
        }
        "input.corners.velocity_max_px_s" => {
            let value = finite_number(path, value, "finite number", "1.0..=20000.0")?;
            if !(1.0..=20_000.0).contains(&value) {
                return Err(invalid_value(path, "finite number", "1.0..=20000.0"));
            }
            let old = config.velocity_max_px_s;
            config.velocity_max_px_s = value;
            Ok((json!(old), json!(value)))
        }
        _ if path.starts_with("input.corners.") => Err(error("unknown_path")),
        _ if known_read_only_path(path) => Err(error("read_only")),
        _ => Err(error("unknown_path")),
    }
}

fn finite_number(
    path: &str,
    value: &Value,
    expected: &'static str,
    range: &'static str,
) -> Result<f64, (u8, Arc<str>)> {
    value
        .as_f64()
        .filter(|number| number.is_finite())
        .ok_or_else(|| invalid_value(path, expected, range))
}

fn invalid_value(path: &str, expected: &str, range: &str) -> (u8, Arc<str>) {
    (
        10,
        Arc::from(
            json!({
                "error": "invalid_value",
                "path": path,
                "expected": expected,
                "range": range,
            })
            .to_string(),
        ),
    )
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
    use super::*;

    #[test]
    fn outbox_evicts_oldest_and_counts_cumulative_loss() {
        let lost = Arc::new(AtomicU64::new(0));
        let (producer, receiver) = outbox(Arc::clone(&lost));
        for sequence in 1..=4 {
            producer.offer(ObservationRecord::FocusChanged {
                keyboard: Some(sequence),
                previous: None,
                exclusive_latch: None,
                event_seq: sequence,
            });
        }
        assert_eq!(lost.load(Ordering::Acquire), 2);
        assert_eq!(receiver.recv().unwrap().event_seq(), 3);
        assert_eq!(receiver.recv().unwrap().event_seq(), 4);
    }

    #[test]
    fn recursive_diff_is_lexical_and_leaf_level() {
        let old = json!({"b": {"z": 1}, "a": 1});
        let new = json!({"b": {"z": 2}, "a": 3});
        let mut changes = BTreeMap::new();
        collect_leaf_diffs("root", &old, &new, &mut changes);
        assert_eq!(
            changes.keys().cloned().collect::<Vec<_>>(),
            ["root.a", "root.b.z"]
        );
    }

    #[test]
    fn property_reducer_coalesces_each_path_and_excludes_operational_leaves() {
        let mut pending = PendingPropChanges::new();
        queue_prop_change(
            &mut pending,
            "surfaces.s2.title".into(),
            json!("old"),
            json!("middle"),
            "wayland.map",
        );
        queue_prop_change(
            &mut pending,
            "surfaces.s2.title".into(),
            json!("middle"),
            json!("new"),
            "wayland.focus",
        );
        queue_prop_change(
            &mut pending,
            "port.event_seq".into(),
            json!(1),
            json!(2),
            "wayland.map",
        );
        assert_eq!(
            pending.remove("surfaces.s2.title"),
            Some((json!("old"), json!("new"), "wayland.map"))
        );
        assert!(pending.is_empty());

        queue_prop_change(
            &mut pending,
            "focus.keyboard".into(),
            Value::Null,
            json!(2),
            "wayland.focus",
        );
        queue_prop_change(
            &mut pending,
            "focus.keyboard".into(),
            json!(2),
            Value::Null,
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
                old: json!(200),
                new: json!(250),
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
            let mut config = CornerConfig::default();
            assert!(
                validate_corner_value(path, &value, &mut config).is_ok(),
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
            let mut config = CornerConfig::default();
            let (rc, body) = validate_corner_value(path, &value, &mut config).unwrap_err();
            assert_eq!(rc, 10);
            assert_eq!(
                serde_json::from_str::<Value>(&body).unwrap()["error"],
                "invalid_value"
            );
        }
    }
}
