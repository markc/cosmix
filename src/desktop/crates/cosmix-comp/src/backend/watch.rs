//! Bounded, seat-independent udev/DRM topology watcher for Rung C.

use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use smithay::reexports::{
    calloop::{EventLoop, Interest, Mode as PollMode, PostAction, generic::Generic},
    udev::{self, EventType, MonitorSocket},
};

use super::{
    kms::{
        AtomicOutputSelection, ConnectorDescription, ConnectorMode, ConnectorRejection, DeviceId,
        KmsRenderCommand, KmsTopology, KmsTopologyEvent, LogicalRect, OutputKey, same_drm_timing,
        scanout_mode_class,
    },
    probe::requested_seat,
    scan::{
        CardCloseOutcome, ConnectorDiff, ConnectorInfo, ConnectorProbe, ConnectorScan,
        ConnectorScanner, ConnectorStatus, DrmMasterState, diff_connector_scans,
    },
};

#[derive(Clone, Debug)]
enum CardEvent {
    Added {
        device: DeviceId,
        path: PathBuf,
        sequence_number: u64,
    },
    Changed {
        device: DeviceId,
        sequence_number: u64,
    },
    Removed {
        device: DeviceId,
        sequence_number: u64,
    },
}

impl CardEvent {
    fn kind(&self) -> &'static str {
        match self {
            Self::Added { .. } => "add",
            Self::Changed { .. } => "change",
            Self::Removed { .. } => "remove",
        }
    }

    fn device(&self) -> DeviceId {
        match self {
            Self::Added { device, .. }
            | Self::Changed { device, .. }
            | Self::Removed { device, .. } => *device,
        }
    }

    fn sequence_number(&self) -> u64 {
        match self {
            Self::Added {
                sequence_number, ..
            }
            | Self::Changed {
                sequence_number, ..
            }
            | Self::Removed {
                sequence_number, ..
            } => *sequence_number,
        }
    }
}

#[derive(Clone, Debug)]
struct TopologyObservation {
    udev_kind: &'static str,
    udev_sequence_number: u64,
    device: DeviceId,
    path: PathBuf,
    connector_probe: Option<ConnectorProbe>,
    connector_query_count: Option<usize>,
    drm_master_state: Option<DrmMasterState>,
    card_close: Option<CardCloseOutcome>,
    output_layouts: BTreeMap<OutputKey, LogicalRect>,
    rejected_connectors: Vec<ConnectorRejection>,
    diffs: Vec<ConnectorDiff>,
    commands: Vec<KmsRenderCommand>,
}

#[derive(Default)]
struct InitialScanSummary {
    drm_master_states: BTreeMap<DeviceId, DrmMasterState>,
    rejected_connectors: Vec<ConnectorRejection>,
    errors: Vec<String>,
}

#[derive(Default)]
struct TopologyTracker {
    paths: BTreeMap<DeviceId, PathBuf>,
    scans: BTreeMap<DeviceId, ConnectorScan>,
    topology: KmsTopology,
}

impl TopologyTracker {
    fn prime<F>(
        &mut self,
        cards: &[(DeviceId, PathBuf)],
        scan: &mut F,
    ) -> Result<InitialScanSummary, String>
    where
        F: FnMut(DeviceId, &Path, ConnectorProbe) -> Result<ConnectorScan, String>,
    {
        let mut summary = InitialScanSummary::default();
        for (device, path) in cards {
            let next = match scan(*device, path, ConnectorProbe::Forced)
                .and_then(|next| validate_scan_identity(&next, *device, path).map(|()| next))
            {
                Ok(next) => next,
                Err(error) => {
                    summary
                        .errors
                        .push(card_scan_error("initial", *device, path, &error));
                    continue;
                }
            };
            if let Some(drm_master_state) = next.drm_master_state() {
                summary.drm_master_states.insert(*device, drm_master_state);
            }
            self.paths.insert(*device, path.clone());
            self.scans.insert(*device, next);
        }
        self.reduce_current()?;
        summary.rejected_connectors = self.topology.rejected_connectors();
        Ok(summary)
    }

    fn handle<F>(&mut self, event: CardEvent, scan: &mut F) -> Result<TopologyObservation, String>
    where
        F: FnMut(DeviceId, &Path, ConnectorProbe) -> Result<ConnectorScan, String>,
    {
        let kind = event.kind();
        let device = event.device();
        let udev_sequence_number = event.sequence_number();
        let path = match &event {
            CardEvent::Added { path, .. } => path.clone(),
            CardEvent::Changed { .. } | CardEvent::Removed { .. } => self
                .paths
                .get(&device)
                .cloned()
                .ok_or_else(|| format!("udev {kind} named unknown DRM dev_t {device}"))?,
        };
        let previous = self
            .scans
            .get(&device)
            .cloned()
            .unwrap_or_else(|| ConnectorScan::empty(device, path.clone()));
        let next = match event {
            CardEvent::Added { .. } | CardEvent::Changed { .. } => {
                scan(device, &path, ConnectorProbe::Forced)?
            }
            CardEvent::Removed { .. } => ConnectorScan::empty(device, path.clone()),
        };
        validate_scan_identity(&next, device, &path)?;
        let connector_probe = next.connector_probe();
        let connector_query_count = connector_probe.map(|_| next.connectors().count());
        let drm_master_state = next.drm_master_state();
        let diffs = diff_connector_scans(&previous, &next);

        match event {
            CardEvent::Added { .. } | CardEvent::Changed { .. } => {
                self.paths.insert(device, path.clone());
                self.scans.insert(device, next);
            }
            CardEvent::Removed { .. } => {
                self.paths.remove(&device);
                self.scans.remove(&device);
            }
        }

        let commands = if diffs.is_empty() {
            Vec::new()
        } else {
            self.reduce_current()?
        };
        let output_layouts = self.topology.output_layouts();
        let rejected_connectors = self.topology.rejected_connectors();
        Ok(TopologyObservation {
            udev_kind: kind,
            udev_sequence_number,
            device,
            path,
            connector_probe,
            connector_query_count,
            drm_master_state,
            card_close: None,
            output_layouts,
            rejected_connectors,
            diffs,
            commands,
        })
    }

    fn reduce_current(&mut self) -> Result<Vec<KmsRenderCommand>, String> {
        let connected = self
            .scans
            .values()
            .flat_map(ConnectorScan::connectors)
            .filter_map(ConnectorInfo::description)
            .collect::<Vec<ConnectorDescription>>();
        let mut discovery_selection =
            |connector: &ConnectorDescription, mode: ConnectorMode| -> Result<_, String> {
                if scanout_mode_class(mode) == 0 {
                    return Err(
                        "read-only topology trace rejected an unsupported scanout mode".to_string(),
                    );
                }
                // This read-only trace does not run atomic admission, so no real
                // CRTC/plane route exists here. Zero is an explicit synthetic
                // sentinel; the exit guard below validates only connector, mode
                // and logical-layout payload fields.
                Ok(AtomicOutputSelection {
                    connector_id: connector.connector_id,
                    crtc_id: 0,
                    primary_plane_id: 0,
                    mode,
                    format: 0,
                    modifier: 0,
                })
            };
        self.topology
            .reduce(
                KmsTopologyEvent::UdevChange(connected),
                &mut discovery_selection,
            )
            .map_err(|error| format!("Rung A topology reducer rejected connector scan: {error}"))
    }
}

fn card_scan_error(kind: &str, device: DeviceId, path: &Path, error: &str) -> String {
    format!(
        "{kind} scan failed for DRM card {} (dev_t {device}): {error}",
        path.display()
    )
}

fn validate_scan_identity(
    scan: &ConnectorScan,
    expected_device: DeviceId,
    expected_path: &Path,
) -> Result<(), String> {
    if scan.device != expected_device {
        return Err(format!(
            "connector scanner returned dev_t {}, expected {expected_device}",
            scan.device
        ));
    }
    if scan.path != expected_path {
        return Err(format!(
            "connector scanner returned path {}, expected {}",
            scan.path.display(),
            expected_path.display()
        ));
    }
    if let Some(connector) = scan
        .connectors()
        .find(|connector| connector.key.device != expected_device)
    {
        return Err(format!(
            "connector {} belongs to dev_t {}, expected {expected_device}",
            connector.key.connector_name, connector.key.device
        ));
    }
    Ok(())
}

#[derive(Clone, Debug)]
struct UdevCardFacts {
    event_type: EventType,
    sequence_number: u64,
    device: Option<DeviceId>,
    path: Option<PathBuf>,
    sysname: OsString,
    seat: OsString,
}

fn is_primary_card_sysname(sysname: &OsStr) -> bool {
    sysname.to_str().is_some_and(|name| {
        name.strip_prefix("card").is_some_and(|suffix| {
            !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
        })
    })
}

fn is_requested_primary_card(facts: &UdevCardFacts, requested_seat: &str) -> bool {
    is_primary_card_sysname(&facts.sysname) && facts.seat == OsStr::new(requested_seat)
}

fn classify_card_event(
    facts: &UdevCardFacts,
    requested_seat: &str,
    known_path: Option<&Path>,
) -> Option<CardEvent> {
    let device = facts.device?;
    if facts.event_type == EventType::Remove {
        return known_path.map(|_| CardEvent::Removed {
            device,
            sequence_number: facts.sequence_number,
        });
    }
    if !is_requested_primary_card(facts, requested_seat) {
        return None;
    }
    match (facts.event_type, known_path) {
        (EventType::Add, Some(_)) => None,
        (EventType::Add | EventType::Change, None) => Some(CardEvent::Added {
            device,
            path: facts.path.clone()?,
            sequence_number: facts.sequence_number,
        }),
        (EventType::Change, Some(_)) => Some(CardEvent::Changed {
            device,
            sequence_number: facts.sequence_number,
        }),
        _ => None,
    }
}

fn facts_from_udev_event(event: &udev::Event) -> UdevCardFacts {
    UdevCardFacts {
        event_type: event.event_type(),
        sequence_number: event.sequence_number(),
        device: event.devnum(),
        path: event.devnode().map(Path::to_path_buf),
        sysname: event.sysname().to_os_string(),
        seat: event
            .property_value("ID_SEAT")
            .unwrap_or_else(|| OsStr::new("seat0"))
            .to_os_string(),
    }
}

struct WatchLoopState {
    requested_seat: String,
    tracker: TopologyTracker,
    scanner: ConnectorScanner,
    observations: Vec<TopologyObservation>,
    errors: Vec<String>,
}

impl WatchLoopState {
    fn new(requested_seat: String) -> Self {
        Self {
            requested_seat,
            tracker: TopologyTracker::default(),
            scanner: ConnectorScanner::default(),
            observations: Vec::new(),
            errors: Vec::new(),
        }
    }

    fn handle_udev(&mut self, facts: UdevCardFacts) {
        let known_path = facts
            .device
            .and_then(|device| self.tracker.paths.get(&device))
            .map(PathBuf::as_path);
        let Some(event) = classify_card_event(&facts, &self.requested_seat, known_path) else {
            return;
        };
        let removed_device = matches!(event, CardEvent::Removed { .. }).then(|| event.device());
        let mut scan = |device, path: &Path, connector_probe| {
            self.scanner
                .scan(device, path, connector_probe)
                .map_err(|error| error.to_string())
        };
        let result = self.tracker.handle(event, &mut scan);
        let card_close = removed_device.and_then(|device| self.scanner.stop_watching(device));
        match result {
            Ok(mut observation) => {
                observation.card_close = card_close;
                self.observations.push(observation);
            }
            Err(error) => self.errors.push(error),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct KmsWatchReport {
    requested_seat: String,
    duration_seconds: u64,
    monitor_started_before_snapshot: bool,
    udev_registered: bool,
    initial_cards: Vec<(DeviceId, PathBuf)>,
    initial_drm_master_states: BTreeMap<DeviceId, DrmMasterState>,
    initial_rejected_connectors: Vec<ConnectorRejection>,
    watch_end_card_closes: BTreeMap<DeviceId, CardCloseOutcome>,
    observations: Vec<TopologyObservation>,
    errors: Vec<String>,
}

impl KmsWatchReport {
    pub(crate) fn success(&self) -> bool {
        self.udev_registered
            && self.monitor_started_before_snapshot
            && self.errors.is_empty()
            && self.udev_sequence_numbers_strictly_increasing()
            && self.exit_criterion_met()
    }

    fn exit_criterion_met(&self) -> bool {
        correct_exit_trace(&self.observations) && self.all_connected_connectors_resolved()
    }

    fn all_connected_connectors_resolved(&self) -> bool {
        self.observations.last().map_or_else(
            || self.initial_rejected_connectors.is_empty(),
            |observation| observation.rejected_connectors.is_empty(),
        )
    }

    fn all_observations_diff_producing(&self) -> bool {
        self.observations
            .iter()
            .all(|observation| !observation.diffs.is_empty())
    }

    fn no_diff_observation_count(&self) -> usize {
        self.observations
            .iter()
            .filter(|observation| observation.diffs.is_empty())
            .count()
    }

    fn udev_sequence_numbers_strictly_increasing(&self) -> bool {
        self.observations
            .windows(2)
            .all(|pair| pair[0].udev_sequence_number < pair[1].udev_sequence_number)
    }

    fn filtered_sequence_gap_observed(&self) -> bool {
        self.observations.windows(2).any(|pair| {
            pair[0]
                .udev_sequence_number
                .checked_add(1)
                .is_none_or(|next| next < pair[1].udev_sequence_number)
        })
    }

    pub(crate) fn to_strict_data(&self) -> String {
        let mut out = String::from("{\n");
        push_field(&mut out, 2, "schema_version", "5", true);
        push_string_field(&mut out, 2, "watcher", "kms-rung-c", true);
        push_field(&mut out, 2, "success", bool_text(self.success()), true);
        push_field(
            &mut out,
            2,
            "exit_criterion_met",
            bool_text(self.exit_criterion_met()),
            true,
        );
        push_field(
            &mut out,
            2,
            "all_connected_connectors_resolved",
            bool_text(self.all_connected_connectors_resolved()),
            true,
        );
        push_field(
            &mut out,
            2,
            "all_observations_diff_producing",
            bool_text(self.all_observations_diff_producing()),
            true,
        );
        push_string_field(&mut out, 2, "requested_seat", &self.requested_seat, true);
        push_field(
            &mut out,
            2,
            "duration_seconds",
            &self.duration_seconds.to_string(),
            true,
        );
        out.push_str("  \"observation_model\": {\n");
        push_string_field(
            &mut out,
            4,
            "topology_source",
            "post-udev-event-card-rescan",
            true,
        );
        push_field(
            &mut out,
            4,
            "no_diff_observations_tolerated_by_exit_trace",
            "true",
            true,
        );
        push_field(
            &mut out,
            4,
            "no_diff_observation_count",
            &self.no_diff_observation_count().to_string(),
            true,
        );
        push_field(&mut out, 4, "no_diff_cause_distinguishable", "false", true);
        push_field(&mut out, 4, "coalesced_transitions_excluded", "false", true);
        push_field(
            &mut out,
            4,
            "event_transition_causation_proven",
            "false",
            true,
        );
        push_field(&mut out, 4, "udev_sequence_numbers_recorded", "true", true);
        push_field(
            &mut out,
            4,
            "udev_sequence_numbers_strictly_increasing",
            bool_text(self.udev_sequence_numbers_strictly_increasing()),
            true,
        );
        push_field(
            &mut out,
            4,
            "filtered_sequence_gap_observed",
            bool_text(self.filtered_sequence_gap_observed()),
            true,
        );
        push_field(
            &mut out,
            4,
            "filtered_sequence_gaps_prove_event_loss",
            "false",
            false,
        );
        out.push_str("  },\n");
        out.push_str("  \"safety\": {\n");
        push_field(&mut out, 4, "drm_open_read_only", "true", true);
        push_field(
            &mut out,
            4,
            "implicit_drm_master_possible_on_open",
            "true",
            true,
        );
        push_field(
            &mut out,
            4,
            "drm_master_acquire_ioctl_attempted",
            "false",
            true,
        );
        push_field(
            &mut out,
            4,
            "drm_master_state_checked_without_acquire_or_drop",
            "true",
            true,
        );
        push_string_field(
            &mut out,
            4,
            "drm_master_check",
            "DRM_IOCTL_AUTH_MAGIC(0)-authority-check",
            true,
        );
        push_field(
            &mut out,
            4,
            "implicit_drm_master_retained_while_card_watched",
            "true",
            true,
        );
        push_string_field(
            &mut out,
            4,
            "connector_probe_policy",
            "force-when-retained-implicit-master-otherwise-cached",
            true,
        );
        push_string_field(
            &mut out,
            4,
            "forced_probe_scope",
            "initial-and-accepted-add-or-change-card-scan-while-current-master",
            true,
        );
        push_field(
            &mut out,
            4,
            "forced_probe_on_non_master_card",
            "false",
            true,
        );
        push_field(
            &mut out,
            4,
            "cached_metadata_may_be_stale_when_not_master",
            "true",
            true,
        );
        push_string_field(
            &mut out,
            4,
            "non_master_observation_limit",
            "cached-only-rung-c-topology-diff-exit-not-met",
            true,
        );
        push_string_field(
            &mut out,
            4,
            "fully_reliable_observation_environment",
            "watcher-retains-implicit-master-before-another-client-opens-card",
            true,
        );
        push_field(
            &mut out,
            4,
            "rung_c_exit_requires_forced_event_probes",
            "true",
            true,
        );
        push_string_field(
            &mut out,
            4,
            "forced_probe_execution",
            "synchronous-standalone-watcher-calloop",
            true,
        );
        push_field(
            &mut out,
            4,
            "forced_probe_may_overrun_watch_duration",
            "true",
            true,
        );
        push_string_field(
            &mut out,
            4,
            "drm_master_release_policy",
            "close-retained-read-only-fd-on-card-remove-or-watch-end",
            true,
        );
        push_field(
            &mut out,
            4,
            "protocol_runtime_active_during_watch",
            "false",
            true,
        );
        push_field(&mut out, 4, "modeset_attempted", "false", true);
        push_field(&mut out, 4, "vulkan_attempted", "false", false);
        out.push_str("  },\n");
        out.push_str("  \"udev\": {\n");
        push_field(
            &mut out,
            4,
            "calloop_registered",
            bool_text(self.udev_registered),
            true,
        );
        push_field(
            &mut out,
            4,
            "monitor_started_before_snapshot",
            bool_text(self.monitor_started_before_snapshot),
            true,
        );
        push_string_field(
            &mut out,
            4,
            "signal",
            "requested-seat-primary-card-add-change-remove",
            false,
        );
        out.push_str("  },\n");
        out.push_str("  \"initial_cards\": [\n");
        for (index, (device, path)) in self.initial_cards.iter().enumerate() {
            let comma = comma(index, self.initial_cards.len());
            out.push_str("    {\n");
            push_field(&mut out, 6, "dev_t", &device.to_string(), true);
            push_string_field(&mut out, 6, "path", &path.to_string_lossy(), true);
            push_optional_string_field(
                &mut out,
                6,
                "drm_master_state",
                self.initial_drm_master_states
                    .get(device)
                    .map(|state| state.as_str()),
                false,
            );
            out.push_str(&format!("    }}{comma}\n"));
        }
        out.push_str("  ],\n");
        push_connector_rejections(
            &mut out,
            2,
            "initial_rejected_connectors",
            &self.initial_rejected_connectors,
            true,
        );
        out.push_str("  \"watch_end_card_closes\": [\n");
        for (index, (device, outcome)) in self.watch_end_card_closes.iter().enumerate() {
            let comma = comma(index, self.watch_end_card_closes.len());
            out.push_str("    {\n");
            push_field(&mut out, 6, "dev_t", &device.to_string(), true);
            push_string_field(&mut out, 6, "outcome", outcome.as_str(), false);
            out.push_str(&format!("    }}{comma}\n"));
        }
        out.push_str("  ],\n");
        out.push_str("  \"observations\": [\n");
        for (index, observation) in self.observations.iter().enumerate() {
            let observation_comma = comma(index, self.observations.len());
            out.push_str("    {\n");
            push_string_field(&mut out, 6, "udev_event", observation.udev_kind, true);
            push_field(
                &mut out,
                6,
                "udev_sequence_number",
                &observation.udev_sequence_number.to_string(),
                true,
            );
            push_field(&mut out, 6, "dev_t", &observation.device.to_string(), true);
            push_string_field(
                &mut out,
                6,
                "path",
                &observation.path.to_string_lossy(),
                true,
            );
            push_optional_string_field(
                &mut out,
                6,
                "drm_master_state",
                observation.drm_master_state.map(DrmMasterState::as_str),
                true,
            );
            push_optional_string_field(
                &mut out,
                6,
                "card_close_outcome",
                observation.card_close.map(CardCloseOutcome::as_str),
                true,
            );
            push_optional_string_field(
                &mut out,
                6,
                "connector_probe_policy",
                observation.connector_probe.map(ConnectorProbe::as_str),
                true,
            );
            push_field(
                &mut out,
                6,
                "connector_query_count",
                &observation
                    .connector_query_count
                    .map_or_else(|| "null".to_string(), |count| count.to_string()),
                true,
            );
            push_string_field(
                &mut out,
                6,
                "reconciliation",
                match (observation.connector_probe, observation.diffs.is_empty()) {
                    (Some(ConnectorProbe::Forced), true) => {
                        "no-change-after-forced-detection-coalescing-not-excluded"
                    }
                    (Some(ConnectorProbe::Forced), false) => "post-probe-topology-diff-observed",
                    (Some(ConnectorProbe::Cached), _) => {
                        "cached-event-scan-rejected-by-exit-criterion"
                    }
                    (None, true) => "no-change-without-connector-query",
                    (None, false) => "event-removal-topology-diff-observed",
                },
                true,
            );
            push_connector_rejections(
                &mut out,
                6,
                "rejected_connectors",
                &observation.rejected_connectors,
                true,
            );
            out.push_str("      \"diffs\": [\n");
            for (diff_index, diff) in observation.diffs.iter().enumerate() {
                let diff_comma = comma(diff_index, observation.diffs.len());
                let connector = diff.connector();
                out.push_str("        {\n");
                push_string_field(&mut out, 10, "kind", diff.kind(), true);
                push_field(
                    &mut out,
                    10,
                    "dev_t",
                    &connector.key.device.to_string(),
                    true,
                );
                push_field(
                    &mut out,
                    10,
                    "connector_id",
                    &connector.connector_id.to_string(),
                    true,
                );
                push_string_field(
                    &mut out,
                    10,
                    "connector_identity",
                    &connector.key.connector_name,
                    true,
                );
                push_string_field(&mut out, 10, "name", &connector.name, true);
                push_string_field(&mut out, 10, "interface", &connector.interface, true);
                push_field(
                    &mut out,
                    10,
                    "interface_id",
                    &connector.interface_id.to_string(),
                    true,
                );
                push_string_field(&mut out, 10, "status", connector.status.as_str(), true);
                let previous_status = match diff {
                    ConnectorDiff::Changed { previous, .. } => Some(previous.status.as_str()),
                    ConnectorDiff::Added { .. } | ConnectorDiff::Removed { .. } => None,
                };
                push_optional_string_field(&mut out, 10, "previous_status", previous_status, true);
                match connector.physical_size_mm {
                    Some((width, height)) => {
                        out.push_str("          \"physical_size_mm\": {\n");
                        push_field(&mut out, 12, "width", &width.to_string(), true);
                        push_field(&mut out, 12, "height", &height.to_string(), false);
                        out.push_str("          },\n");
                    }
                    None => push_field(&mut out, 10, "physical_size_mm", "null", true),
                }
                push_field(
                    &mut out,
                    10,
                    "mode_count",
                    &connector.modes.len().to_string(),
                    false,
                );
                out.push_str(&format!("        }}{diff_comma}\n"));
            }
            out.push_str("      ],\n");
            out.push_str("      \"reducer_commands\": [\n");
            for (command_index, command) in observation.commands.iter().enumerate() {
                let command_comma = comma(command_index, observation.commands.len());
                let (kind, generation, key) = command_summary(command);
                out.push_str("        {\n");
                push_string_field(&mut out, 10, "kind", kind, true);
                push_field(&mut out, 10, "generation", &generation.to_string(), true);
                match key {
                    Some(key) => {
                        push_field(&mut out, 10, "dev_t", &key.device.to_string(), true);
                        push_field(
                            &mut out,
                            10,
                            "connector_identity",
                            &format!("\"{}\"", strict_string(&key.connector_name)),
                            false,
                        );
                    }
                    None => {
                        push_field(&mut out, 10, "dev_t", "null", true);
                        push_field(&mut out, 10, "connector_identity", "null", false);
                    }
                }
                out.push_str(&format!("        }}{command_comma}\n"));
            }
            out.push_str("      ]\n");
            out.push_str(&format!("    }}{observation_comma}\n"));
        }
        out.push_str("  ],\n");
        push_string_list(&mut out, 2, "errors", &self.errors, false);
        out.push_str("}\n");
        out
    }
}

fn incorporate_initial_scan(
    report: &mut KmsWatchReport,
    result: Result<InitialScanSummary, String>,
) -> bool {
    match result {
        Ok(summary) => {
            report.initial_drm_master_states = summary.drm_master_states;
            report.initial_rejected_connectors = summary.rejected_connectors;
            report.errors.extend(summary.errors);
            true
        }
        Err(error) => {
            report.errors.push(error);
            false
        }
    }
}

fn correct_exit_trace(trace: &[TopologyObservation]) -> bool {
    trace_has_exactly_four_diff_command_pairs(trace)
        && trace_uses_required_connector_probe_policy(trace)
        && trace_has_expected_card_event_order(trace)
        && trace_has_expected_diff_transitions(trace)
        && trace_keeps_card_identity(trace)
        && trace_keeps_connector_identity(trace)
        && trace_connectors_belong_to_observed_card(trace)
        && trace_diff_chain_is_contiguous(trace)
        && trace_commands_match_transitions(trace)
        && trace_command_keys_match(trace)
        && trace_command_mode_and_layout_match(trace)
        && trace_generations_are_consecutive(trace)
}

fn trace_uses_required_connector_probe_policy(trace: &[TopologyObservation]) -> bool {
    let Some(target) = trace
        .iter()
        .find(|observation| !observation.diffs.is_empty())
    else {
        return false;
    };
    trace.iter().all(|observation| {
        let is_target = observation.device == target.device && observation.path == target.path;
        match (observation.udev_kind, is_target) {
            ("add" | "change", true) => {
                observation.connector_probe == Some(ConnectorProbe::Forced)
                    && observation.drm_master_state == Some(DrmMasterState::RetainedImplicit)
                    && observation.card_close.is_none()
            }
            ("add" | "change", false) => {
                matches!(
                    (
                        observation.connector_probe,
                        observation.drm_master_state,
                        observation.card_close,
                    ),
                    (
                        Some(ConnectorProbe::Forced),
                        Some(DrmMasterState::RetainedImplicit),
                        None,
                    ) | (
                        Some(ConnectorProbe::Cached),
                        Some(DrmMasterState::NotMaster),
                        None,
                    )
                )
            }
            ("remove", true) => {
                observation.connector_probe.is_none()
                    && observation.drm_master_state.is_none()
                    && observation.card_close
                        == Some(CardCloseOutcome::ClosedRetainedImplicitMaster)
            }
            ("remove", false) => {
                observation.connector_probe.is_none()
                    && observation.drm_master_state.is_none()
                    && observation.card_close.is_some()
            }
            _ => false,
        }
    })
}

fn diff_trace(trace: &[TopologyObservation]) -> Vec<&TopologyObservation> {
    trace
        .iter()
        .filter(|observation| !observation.diffs.is_empty())
        .collect()
}

fn trace_has_exactly_four_diff_command_pairs(trace: &[TopologyObservation]) -> bool {
    diff_trace(trace).len() == 4
        && trace.iter().all(|observation| {
            (observation.diffs.is_empty() && observation.commands.is_empty())
                || (observation.diffs.len() == 1 && observation.commands.len() == 1)
        })
}

fn trace_has_expected_card_event_order(trace: &[TopologyObservation]) -> bool {
    diff_trace(trace)
        .iter()
        .take(4)
        .map(|observation| observation.udev_kind)
        .eq(["add", "change", "change", "remove"])
}

fn trace_has_expected_diff_transitions(trace: &[TopologyObservation]) -> bool {
    let trace = diff_trace(trace);
    matches!(
        (
            trace.first().and_then(|item| item.diffs.first()),
            trace.get(1).and_then(|item| item.diffs.first()),
            trace.get(2).and_then(|item| item.diffs.first()),
            trace.get(3).and_then(|item| item.diffs.first()),
        ),
        (
            Some(ConnectorDiff::Added { connector: added }),
            Some(ConnectorDiff::Changed {
                previous: disconnected_previous,
                connector: disconnected,
            }),
            Some(ConnectorDiff::Changed {
                previous: connected_previous,
                connector: reconnected,
            }),
            Some(ConnectorDiff::Removed { connector: removed }),
        ) if added.status == ConnectorStatus::Connected
            && disconnected_previous.status == ConnectorStatus::Connected
            && disconnected.status == ConnectorStatus::Disconnected
            && connected_previous.status == ConnectorStatus::Disconnected
            && reconnected.status == ConnectorStatus::Connected
            && removed.status == ConnectorStatus::Connected
    )
}

fn trace_keeps_card_identity(trace: &[TopologyObservation]) -> bool {
    let trace = diff_trace(trace);
    let Some(first) = trace.first() else {
        return false;
    };
    trace
        .iter()
        .all(|observation| observation.device == first.device && observation.path == first.path)
}

fn trace_keeps_connector_identity(trace: &[TopologyObservation]) -> bool {
    let trace = diff_trace(trace);
    let mut keys = trace
        .iter()
        .flat_map(|observation| observation.diffs.iter().map(|diff| &diff.connector().key));
    let Some(first) = keys.next() else {
        return false;
    };
    keys.all(|key| key == first)
}

fn trace_connectors_belong_to_observed_card(trace: &[TopologyObservation]) -> bool {
    diff_trace(trace).iter().all(|observation| {
        observation
            .diffs
            .iter()
            .all(|diff| diff.connector().key.device == observation.device)
    })
}

fn trace_diff_chain_is_contiguous(trace: &[TopologyObservation]) -> bool {
    let trace = diff_trace(trace);
    matches!(
        (
            trace.first().and_then(|item| item.diffs.first()),
            trace.get(1).and_then(|item| item.diffs.first()),
            trace.get(2).and_then(|item| item.diffs.first()),
            trace.get(3).and_then(|item| item.diffs.first()),
        ),
        (
            Some(ConnectorDiff::Added { connector: added }),
            Some(ConnectorDiff::Changed {
                previous: disconnect_previous,
                connector: disconnected,
            }),
            Some(ConnectorDiff::Changed {
                previous: reconnect_previous,
                connector: reconnected,
            }),
            Some(ConnectorDiff::Removed { connector: removed }),
        ) if same_connector_payload(added, disconnect_previous)
            && same_connector_payload(disconnected, reconnect_previous)
            && same_connector_payload(reconnected, removed)
    )
}

fn same_connector_payload(left: &ConnectorInfo, right: &ConnectorInfo) -> bool {
    left.connector_id == right.connector_id
        && left.name == right.name
        && left.interface == right.interface
        && left.interface_id == right.interface_id
        && left.status == right.status
        && left.physical_size_mm == right.physical_size_mm
        && left.modes == right.modes
}

fn trace_commands_match_transitions(trace: &[TopologyObservation]) -> bool {
    let trace = diff_trace(trace);
    matches!(
        (
            trace.first().and_then(|item| item.commands.first()),
            trace.get(1).and_then(|item| item.commands.first()),
            trace.get(2).and_then(|item| item.commands.first()),
            trace.get(3).and_then(|item| item.commands.first()),
        ),
        (
            Some(KmsRenderCommand::AddOutput { .. }),
            Some(KmsRenderCommand::RemoveOutput { .. }),
            Some(KmsRenderCommand::AddOutput { .. }),
            Some(KmsRenderCommand::RemoveOutput { .. }),
        )
    )
}

fn trace_command_keys_match(trace: &[TopologyObservation]) -> bool {
    diff_trace(trace).iter().all(|observation| {
        let Some(diff) = observation.diffs.first() else {
            return false;
        };
        observation
            .commands
            .first()
            .and_then(command_key)
            .is_some_and(|key| key == &diff.connector().key)
    })
}

/// Validate only payload fields the read-only watcher actually discovers.
/// CRTC and primary-plane route IDs are deliberately excluded: obtaining them
/// requires atomic admission, while this trace uses named zero sentinels.
fn trace_command_mode_and_layout_match(trace: &[TopologyObservation]) -> bool {
    diff_trace(trace).iter().all(|observation| {
        let Some(diff) = observation.diffs.first() else {
            return false;
        };
        match observation.commands.first() {
            Some(
                KmsRenderCommand::AddOutput { output, .. }
                | KmsRenderCommand::ChangeOutput { output, .. },
            ) => {
                diff.connector().modes.contains(&output.connector_mode)
                    && output.connector_id == diff.connector().connector_id
                    && output.display.connector_id == output.connector_id
                    && same_drm_timing(output.display.mode, output.connector_mode)
                    && output.logical_rect.width
                        == i32::try_from(output.display.mode.width).unwrap_or(i32::MAX)
                    && output.logical_rect.height
                        == i32::try_from(output.display.mode.height).unwrap_or(i32::MAX)
                    && observation.output_layouts.get(&output.key) == Some(&output.logical_rect)
            }
            Some(KmsRenderCommand::RemoveOutput { .. }) => true,
            Some(KmsRenderCommand::Suspend { .. } | KmsRenderCommand::Resume { .. }) | None => {
                false
            }
        }
    })
}

fn trace_generations_are_consecutive(trace: &[TopologyObservation]) -> bool {
    let generations = diff_trace(trace)
        .iter()
        .take(4)
        .filter_map(|observation| observation.commands.first().map(command_generation))
        .collect::<Vec<_>>();
    generations.len() == 4
        && generations
            .windows(2)
            .all(|pair| pair[0].checked_add(1) == Some(pair[1]))
}

fn initialise_udev(seat: &str) -> std::io::Result<(MonitorSocket, Vec<(DeviceId, PathBuf)>)> {
    initialise_udev_with(
        || {
            udev::MonitorBuilder::new()?
                .match_subsystem("drm")?
                .listen()
        },
        || enumerate_initial_cards(seat),
    )
}

fn initialise_udev_with<M, E>(
    start_monitor: impl FnOnce() -> Result<M, E>,
    enumerate: impl FnOnce() -> Result<Vec<(DeviceId, PathBuf)>, E>,
) -> Result<(M, Vec<(DeviceId, PathBuf)>), E> {
    let monitor = start_monitor()?;
    let cards = enumerate()?;
    Ok((monitor, cards))
}

fn enumerate_initial_cards(seat: &str) -> std::io::Result<Vec<(DeviceId, PathBuf)>> {
    let mut enumerator = udev::Enumerator::new()?;
    enumerator.match_subsystem("drm")?;
    let mut cards = enumerator
        .scan_devices()?
        .filter_map(|device| {
            let facts = UdevCardFacts {
                event_type: EventType::Add,
                sequence_number: 0,
                device: device.devnum(),
                path: device.devnode().map(Path::to_path_buf),
                sysname: device.sysname().to_os_string(),
                seat: device
                    .property_value("ID_SEAT")
                    .unwrap_or_else(|| OsStr::new("seat0"))
                    .to_os_string(),
            };
            is_requested_primary_card(&facts, seat)
                .then(|| facts.device.zip(facts.path))
                .flatten()
        })
        .collect::<Vec<_>>();
    cards.sort_by(|left, right| left.1.cmp(&right.1));
    Ok(cards)
}

pub(crate) fn run(duration: Duration) -> KmsWatchReport {
    let requested_seat = requested_seat();
    let duration_seconds = duration.as_secs();
    let mut report = KmsWatchReport {
        requested_seat: requested_seat.clone(),
        duration_seconds,
        monitor_started_before_snapshot: false,
        udev_registered: false,
        initial_cards: Vec::new(),
        initial_drm_master_states: BTreeMap::new(),
        initial_rejected_connectors: Vec::new(),
        watch_end_card_closes: BTreeMap::new(),
        observations: Vec::new(),
        errors: Vec::new(),
    };
    let mut event_loop = match EventLoop::<WatchLoopState>::try_new() {
        Ok(event_loop) => event_loop,
        Err(error) => {
            report
                .errors
                .push(format!("KMS watch calloop construction failed: {error}"));
            return report;
        }
    };
    let (udev_monitor, initial_cards) = match initialise_udev(&requested_seat) {
        Ok(parts) => parts,
        Err(error) => {
            report.errors.push(format!(
                "udev monitor/enumeration for seat {requested_seat} failed: {error}"
            ));
            return report;
        }
    };
    report.initial_cards = initial_cards;
    report.monitor_started_before_snapshot = true;

    let mut state = WatchLoopState::new(requested_seat.clone());
    let scanner = &mut state.scanner;
    let tracker = &mut state.tracker;
    let mut scan = |device, path: &Path, connector_probe| {
        scanner
            .scan(device, path, connector_probe)
            .map_err(|error| error.to_string())
    };
    let initial_scan = tracker.prime(&report.initial_cards, &mut scan);
    if !incorporate_initial_scan(&mut report, initial_scan) {
        report.watch_end_card_closes = state.scanner.stop_all();
        return report;
    }
    if let Err(error) = event_loop.handle().insert_source(
        Generic::new(udev_monitor, Interest::READ, PollMode::Level),
        |_, monitor, state: &mut WatchLoopState| {
            for event in monitor.iter() {
                state.handle_udev(facts_from_udev_event(&event));
            }
            Ok(PostAction::Continue)
        },
    ) {
        report
            .errors
            .push(format!("udev calloop registration failed: {error}"));
        report.watch_end_card_closes = state.scanner.stop_all();
        return report;
    }
    report.udev_registered = true;

    let deadline = Instant::now() + duration;
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        if let Err(error) = event_loop.dispatch(Some(remaining), &mut state) {
            state
                .errors
                .push(format!("KMS watch calloop dispatch failed: {error}"));
            break;
        }
    }

    report.observations = state.observations;
    report.errors.extend(state.errors);
    report.watch_end_card_closes = state.scanner.stop_all();
    report
}

fn command_summary(command: &KmsRenderCommand) -> (&'static str, u64, Option<&OutputKey>) {
    match command {
        KmsRenderCommand::AddOutput { generation, output } => {
            ("add-output", *generation, Some(&output.key))
        }
        KmsRenderCommand::ChangeOutput { generation, output } => {
            ("change-output", *generation, Some(&output.key))
        }
        KmsRenderCommand::RemoveOutput { generation, key } => {
            ("remove-output", *generation, Some(key))
        }
        KmsRenderCommand::Suspend { generation } => ("suspend", *generation, None),
        KmsRenderCommand::Resume { generation } => ("resume", *generation, None),
    }
}

fn command_generation(command: &KmsRenderCommand) -> u64 {
    command_summary(command).1
}

fn command_key(command: &KmsRenderCommand) -> Option<&OutputKey> {
    command_summary(command).2
}

fn bool_text(value: bool) -> &'static str {
    if value { "true" } else { "false" }
}

fn comma(index: usize, len: usize) -> &'static str {
    if index + 1 == len { "" } else { "," }
}

fn push_field(out: &mut String, indent: usize, key: &str, value: &str, comma: bool) {
    out.push_str(&" ".repeat(indent));
    out.push_str(&format!(
        "\"{key}\": {value}{}\n",
        if comma { "," } else { "" }
    ));
}

fn push_string_field(out: &mut String, indent: usize, key: &str, value: &str, comma: bool) {
    push_field(
        out,
        indent,
        key,
        &format!("\"{}\"", strict_string(value)),
        comma,
    );
}

fn push_optional_string_field(
    out: &mut String,
    indent: usize,
    key: &str,
    value: Option<&str>,
    comma: bool,
) {
    match value {
        Some(value) => push_string_field(out, indent, key, value, comma),
        None => push_field(out, indent, key, "null", comma),
    }
}

fn push_connector_rejections(
    out: &mut String,
    indent: usize,
    key: &str,
    rejections: &[ConnectorRejection],
    trailing_comma: bool,
) {
    out.push_str(&" ".repeat(indent));
    out.push_str(&format!("\"{key}\": [\n"));
    for (index, rejection) in rejections.iter().enumerate() {
        let item_indent = indent + 2;
        out.push_str(&" ".repeat(item_indent));
        out.push_str("{\n");
        push_field(
            out,
            item_indent + 2,
            "dev_t",
            &rejection.key.device.to_string(),
            true,
        );
        push_string_field(
            out,
            item_indent + 2,
            "connector_identity",
            &rejection.key.connector_name,
            true,
        );
        push_field(
            out,
            item_indent + 2,
            "candidate_mode_count",
            &rejection.candidate_mode_count.to_string(),
            true,
        );
        push_field(
            out,
            item_indent + 2,
            "rejected_mode_count",
            &rejection.rejected_mode_count.to_string(),
            true,
        );
        match rejection.highest_ranked_candidate {
            Some(mode) => {
                out.push_str(&" ".repeat(item_indent + 2));
                out.push_str("\"highest_ranked_candidate\": {\n");
                push_field(out, item_indent + 4, "width", &mode.width.to_string(), true);
                push_field(
                    out,
                    item_indent + 4,
                    "height",
                    &mode.height.to_string(),
                    true,
                );
                push_field(
                    out,
                    item_indent + 4,
                    "refresh_millihz",
                    &mode.refresh_millihz.to_string(),
                    false,
                );
                out.push_str(&" ".repeat(item_indent + 2));
                out.push_str("},\n");
            }
            None => push_field(
                out,
                item_indent + 2,
                "highest_ranked_candidate",
                "null",
                true,
            ),
        }
        push_string_field(
            out,
            item_indent + 2,
            "reason_code",
            rejection.reason_code,
            true,
        );
        push_string_field(out, item_indent + 2, "reason", &rejection.reason, false);
        out.push_str(&" ".repeat(item_indent));
        out.push_str(&format!("}}{}\n", comma(index, rejections.len())));
    }
    out.push_str(&" ".repeat(indent));
    out.push_str(if trailing_comma { "],\n" } else { "]\n" });
}

fn push_string_list(out: &mut String, indent: usize, key: &str, values: &[String], comma: bool) {
    let values = values
        .iter()
        .map(|value| format!("\"{}\"", strict_string(value)))
        .collect::<Vec<_>>()
        .join(", ");
    push_field(out, indent, key, &format!("[{values}]"), comma);
}

fn strict_string(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric()
                || matches!(
                    character,
                    ' ' | '!'
                        | '#'
                        | '%'
                        | '&'
                        | '('
                        | ')'
                        | '+'
                        | ','
                        | '-'
                        | '.'
                        | '/'
                        | ':'
                        | '<'
                        | '='
                        | '>'
                        | '?'
                        | '@'
                        | '['
                        | ']'
                        | '^'
                        | '_'
                )
            {
                character
            } else {
                '?'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use crate::backend::kms::ConnectorModeTuple;

    use super::*;

    fn connector(status: ConnectorStatus, preferred: bool) -> ConnectorInfo {
        ConnectorInfo {
            key: OutputKey {
                device: 200,
                connector_name: "Virtual-1".into(),
            },
            connector_id: 31,
            name: "Virtual-1".into(),
            interface: "Virtual".into(),
            interface_id: 1,
            status,
            physical_size_mm: None,
            modes: vec![ConnectorMode {
                width: 1920,
                height: 1080,
                refresh_millihz: 60_000,
                preferred,
                clock_khz: 148_500,
                hsync: (2008, 2052, 2200),
                vsync: (1084, 1089, 1125),
                hskew: 0,
                vscan: 0,
                flags: 0,
            }],
        }
    }

    fn rejected_connector(device: DeviceId, connector_identity: &str) -> ConnectorRejection {
        ConnectorRejection {
            key: OutputKey {
                device,
                connector_name: connector_identity.into(),
            },
            candidate_mode_count: 1,
            rejected_mode_count: 1,
            highest_ranked_candidate: Some(ConnectorModeTuple {
                width: 1920,
                height: 1080,
                refresh_millihz: 60_000,
            }),
            reason_code: "display-selection-rejected",
            reason: "the selected DRM mode is interlaced".into(),
        }
    }

    fn scan(connector: Option<ConnectorInfo>) -> ConnectorScan {
        ConnectorScan::from_connectors(
            200,
            "/dev/dri/card1".into(),
            ConnectorProbe::Forced,
            DrmMasterState::RetainedImplicit,
            connector,
        )
    }

    #[test]
    fn one_tracker_incarnation_delivers_add_change_remove_through_the_reducer() {
        let path = PathBuf::from("/dev/dri/card1");
        let mut tracker = TopologyTracker::default();
        tracker
            .prime(&[], &mut |_, _, _| unreachable!("no initial cards"))
            .expect("empty topology");

        let added = tracker
            .handle(
                CardEvent::Added {
                    device: 200,
                    path: path.clone(),
                    sequence_number: 100,
                },
                &mut |_, _, _| Ok(scan(Some(connector(ConnectorStatus::Connected, true)))),
            )
            .expect("add");
        assert!(matches!(
            added.diffs.as_slice(),
            [ConnectorDiff::Added { .. }]
        ));
        assert!(matches!(
            added.commands.as_slice(),
            [KmsRenderCommand::AddOutput { generation: 1, .. }]
        ));

        let changed = tracker
            .handle(
                CardEvent::Changed {
                    device: 200,
                    sequence_number: 101,
                },
                &mut |_, _, _| Ok(scan(Some(connector(ConnectorStatus::Disconnected, true)))),
            )
            .expect("change");
        assert!(matches!(
            changed.diffs.as_slice(),
            [ConnectorDiff::Changed { .. }]
        ));
        assert!(matches!(
            changed.commands.as_slice(),
            [KmsRenderCommand::RemoveOutput { generation: 2, .. }]
        ));

        let reconnected = tracker
            .handle(
                CardEvent::Changed {
                    device: 200,
                    sequence_number: 102,
                },
                &mut |_, _, _| Ok(scan(Some(connector(ConnectorStatus::Connected, false)))),
            )
            .expect("reconnect");
        assert!(matches!(
            reconnected.diffs.as_slice(),
            [ConnectorDiff::Changed { .. }]
        ));
        assert!(matches!(
            reconnected.commands.as_slice(),
            [KmsRenderCommand::AddOutput { generation: 3, .. }]
        ));

        let removed = tracker
            .handle(
                CardEvent::Removed {
                    device: 200,
                    sequence_number: 103,
                },
                &mut |_, _, _| unreachable!("remove must not rescan a vanished node"),
            )
            .expect("remove");
        assert!(matches!(
            removed.diffs.as_slice(),
            [ConnectorDiff::Removed { .. }]
        ));
        assert!(matches!(
            removed.commands.as_slice(),
            [KmsRenderCommand::RemoveOutput { generation: 4, .. }]
        ));
    }

    #[test]
    fn initial_snapshot_requests_detection_and_records_a_non_master_cached_read() {
        let mut tracker = TopologyTracker::default();
        let summary = tracker
            .prime(
                &[(200, PathBuf::from("/dev/dri/card1"))],
                &mut |device, path, requested_probe| {
                    assert_eq!(requested_probe, ConnectorProbe::Forced);
                    Ok(ConnectorScan::from_connectors(
                        device,
                        path.to_path_buf(),
                        ConnectorProbe::Cached,
                        DrmMasterState::NotMaster,
                        [connector(ConnectorStatus::Connected, true)],
                    ))
                },
            )
            .expect("cached non-master initial snapshot");
        assert_eq!(
            summary.drm_master_states,
            BTreeMap::from([(200, DrmMasterState::NotMaster)])
        );
    }

    #[test]
    fn initial_unresolved_connected_connector_is_retained_without_an_observation() {
        let mut tracker = TopologyTracker::default();
        let mut interlaced = connector(ConnectorStatus::Connected, true);
        interlaced.modes[0].flags = 1 << 4;

        let summary = tracker
            .prime(
                &[(200, PathBuf::from("/dev/dri/card1"))],
                &mut |device, path, requested_probe| {
                    assert_eq!(requested_probe, ConnectorProbe::Forced);
                    Ok(ConnectorScan::from_connectors(
                        device,
                        path.to_path_buf(),
                        ConnectorProbe::Forced,
                        DrmMasterState::RetainedImplicit,
                        [interlaced.clone()],
                    ))
                },
            )
            .expect("initial scan remains operational");

        let [rejection] = summary.rejected_connectors.as_slice() else {
            panic!("the initial rejection must be first-class data");
        };
        assert_eq!(rejection.key.device, 200);
        assert_eq!(rejection.key.connector_name, "Virtual-1");
        assert_eq!(rejection.candidate_mode_count, 1);
        assert_eq!(rejection.rejected_mode_count, 1);
        assert_eq!(rejection.reason_code, "display-selection-rejected");
        assert!(tracker.topology.output_layouts().is_empty());
    }

    #[test]
    fn event_rescan_records_an_honest_cached_result_when_not_master() {
        let mut tracker = TopologyTracker::default();
        tracker
            .prime(&[], &mut |_, _, _| unreachable!("no initial cards"))
            .expect("empty topology");

        let observation = tracker
            .handle(
                CardEvent::Added {
                    device: 200,
                    path: "/dev/dri/card1".into(),
                    sequence_number: 100,
                },
                &mut |device, path, requested_probe| {
                    assert_eq!(requested_probe, ConnectorProbe::Forced);
                    Ok(ConnectorScan::from_connectors(
                        device,
                        path.to_path_buf(),
                        ConnectorProbe::Cached,
                        DrmMasterState::NotMaster,
                        [connector(ConnectorStatus::Connected, true)],
                    ))
                },
            )
            .expect("cached event metadata remains reportable");

        assert_eq!(observation.connector_probe, Some(ConnectorProbe::Cached));
        assert_eq!(
            observation.drm_master_state,
            Some(DrmMasterState::NotMaster)
        );
        assert!(!trace_uses_required_connector_probe_policy(&[observation]));
    }

    #[test]
    fn exit_trace_rejects_wrong_card_event_order_as_the_sole_falsifier() {
        let mut report = complete_report();
        report.observations[0].udev_kind = "change";

        assert_all_exit_guards_except(&report.observations, trace_has_expected_card_event_order);
        assert!(!report.exit_criterion_met());
    }

    #[test]
    fn exit_trace_rejects_cached_event_scan_as_the_sole_falsifier() {
        let mut report = complete_report();
        report.observations[1].connector_probe = Some(ConnectorProbe::Cached);

        assert_all_exit_guards_except(
            &report.observations,
            trace_uses_required_connector_probe_policy,
        );
        assert!(!report.exit_criterion_met());
    }

    #[test]
    fn exit_trace_rejects_forced_probe_without_retained_master_as_the_sole_falsifier() {
        let mut report = complete_report();
        report.observations[1].drm_master_state = Some(DrmMasterState::NotMaster);

        assert_all_exit_guards_except(
            &report.observations,
            trace_uses_required_connector_probe_policy,
        );
        assert!(!report.exit_criterion_met());
    }

    #[test]
    fn exit_trace_rejects_unproven_master_release_on_remove_as_the_sole_falsifier() {
        let mut report = complete_report();
        report.observations[3].card_close = Some(CardCloseOutcome::ClosedNonMaster);

        assert_all_exit_guards_except(
            &report.observations,
            trace_uses_required_connector_probe_policy,
        );
        assert!(!report.exit_criterion_met());
    }

    #[test]
    fn exit_trace_rejects_wrong_diff_cardinality_as_the_sole_falsifier() {
        let mut report = complete_report();
        let duplicate = report.observations[0].diffs[0].clone();
        report.observations[0].diffs.push(duplicate);

        assert_all_exit_guards_except(
            &report.observations,
            trace_has_exactly_four_diff_command_pairs,
        );
        assert!(!report.exit_criterion_met());
    }

    #[test]
    fn exit_trace_rejects_unexpected_diff_observation_as_the_sole_falsifier() {
        let mut report = complete_report();
        let mut unexpected = report.observations[3].clone();
        unexpected.udev_sequence_number = 108;
        report.observations.push(unexpected);

        assert_all_exit_guards_except(
            &report.observations,
            trace_has_exactly_four_diff_command_pairs,
        );
        assert!(!report.exit_criterion_met());
    }

    #[test]
    fn exit_trace_rejects_wrong_status_transition_as_the_sole_falsifier() {
        let mut report = complete_report();
        changed_next_mut(&mut report.observations[1]).status = ConnectorStatus::Unknown;
        changed_previous_mut(&mut report.observations[2]).status = ConnectorStatus::Unknown;

        assert_all_exit_guards_except(&report.observations, trace_has_expected_diff_transitions);
        assert!(!report.exit_criterion_met());
    }

    #[test]
    fn exit_trace_rejects_changed_card_identity_as_the_sole_falsifier() {
        let mut report = complete_report();
        report.observations[3].path = "/dev/dri/card2".into();

        assert_all_exit_guards_except(&report.observations, trace_keeps_card_identity);
        assert!(!report.exit_criterion_met());
    }

    #[test]
    fn exit_trace_rejects_changed_connector_identity_as_the_sole_falsifier() {
        let mut report = complete_report();
        for observation in &mut report.observations[2..] {
            set_diff_connector_name(&mut observation.diffs[0], "Virtual-2");
            set_command_connector_name(&mut observation.commands[0], "Virtual-2");
            set_layout_connector_name(&mut observation.output_layouts, "Virtual-2");
        }

        assert_all_exit_guards_except(&report.observations, trace_keeps_connector_identity);
        assert!(!report.exit_criterion_met());
    }

    #[test]
    fn exit_trace_rejects_connector_from_another_card_as_the_sole_falsifier() {
        let mut report = complete_report();
        for observation in &mut report.observations {
            set_diff_connector_device(&mut observation.diffs[0], 201);
            set_command_connector_device(&mut observation.commands[0], 201);
            set_layout_connector_device(&mut observation.output_layouts, 201);
        }

        assert_all_exit_guards_except(
            &report.observations,
            trace_connectors_belong_to_observed_card,
        );
        assert!(!report.exit_criterion_met());
    }

    #[test]
    fn exit_trace_rejects_non_contiguous_diff_payload_as_the_sole_falsifier() {
        let mut report = complete_report();
        changed_previous_mut(&mut report.observations[1]).name = "stale-name".into();

        assert_all_exit_guards_except(&report.observations, trace_diff_chain_is_contiguous);
        assert!(!report.exit_criterion_met());
    }

    #[test]
    fn exit_trace_rejects_wrong_reducer_command_as_the_sole_falsifier() {
        let mut report = complete_report();
        let generation = command_generation(&report.observations[1].commands[0]);
        let output = match &report.observations[0].commands[0] {
            KmsRenderCommand::AddOutput { output, .. } => output.clone(),
            _ => unreachable!("complete trace initial command"),
        };
        report.observations[1].commands[0] = KmsRenderCommand::AddOutput { generation, output };
        let (key, rect) = match &report.observations[1].commands[0] {
            KmsRenderCommand::AddOutput { output, .. } => (output.key.clone(), output.logical_rect),
            _ => unreachable!("replacement command"),
        };
        report.observations[1].output_layouts.insert(key, rect);

        assert_all_exit_guards_except(&report.observations, trace_commands_match_transitions);
        assert!(!report.exit_criterion_met());
    }

    #[test]
    fn exit_trace_rejects_wrong_reducer_key_as_the_sole_falsifier() {
        let mut report = complete_report();
        set_command_connector_name(&mut report.observations[2].commands[0], "Virtual-2");
        set_layout_connector_name(&mut report.observations[2].output_layouts, "Virtual-2");

        assert_all_exit_guards_except(&report.observations, trace_command_keys_match);
        assert!(!report.exit_criterion_met());
    }

    #[test]
    fn exit_trace_rejects_wrong_reducer_mode_as_the_sole_falsifier() {
        let mut report = complete_report();
        match &mut report.observations[2].commands[0] {
            KmsRenderCommand::AddOutput { output, .. } => output.connector_mode.clock_khz += 1,
            _ => unreachable!("complete trace reconnect command"),
        }

        assert_all_exit_guards_except(&report.observations, trace_command_mode_and_layout_match);
        assert!(!report.exit_criterion_met());
    }

    #[test]
    fn exit_trace_rejects_wrong_reducer_connector_id_as_the_sole_falsifier() {
        let mut report = complete_report();
        match &mut report.observations[2].commands[0] {
            KmsRenderCommand::AddOutput { output, .. } => {
                output.connector_id += 1;
                output.display.connector_id = output.connector_id;
            }
            _ => unreachable!("complete trace reconnect command"),
        }

        assert_all_exit_guards_except(&report.observations, trace_command_mode_and_layout_match);
        assert!(!report.exit_criterion_met());
    }

    #[test]
    fn discovery_trace_explicitly_excludes_synthetic_route_ids() {
        let mut report = complete_report();
        match &mut report.observations[2].commands[0] {
            KmsRenderCommand::AddOutput { output, .. } => {
                output.display.crtc_id = 99;
                output.display.primary_plane_id = 101;
            }
            _ => unreachable!("complete trace reconnect command"),
        }

        assert!(trace_command_mode_and_layout_match(&report.observations));
        assert!(report.exit_criterion_met());
    }

    #[test]
    fn exit_trace_rejects_layout_origin_not_produced_by_reducer_as_the_sole_falsifier() {
        let mut report = complete_report();
        match &mut report.observations[2].commands[0] {
            KmsRenderCommand::AddOutput { output, .. } => output.logical_rect.x += 1,
            _ => unreachable!("complete trace reconnect command"),
        }

        assert_all_exit_guards_except(&report.observations, trace_command_mode_and_layout_match);
        assert!(!report.exit_criterion_met());
    }

    #[test]
    fn exit_trace_rejects_layout_size_inconsistent_with_mode_as_the_sole_falsifier() {
        let mut report = complete_report();
        match &mut report.observations[2].commands[0] {
            KmsRenderCommand::AddOutput { output, .. } => output.logical_rect.width += 1,
            _ => unreachable!("complete trace reconnect command"),
        }

        assert_all_exit_guards_except(&report.observations, trace_command_mode_and_layout_match);
        assert!(!report.exit_criterion_met());
    }

    #[test]
    fn exit_trace_rejects_non_consecutive_generation_as_the_sole_falsifier() {
        let mut report = complete_report();
        set_command_generation(&mut report.observations[3].commands[0], 9);

        assert_all_exit_guards_except(&report.observations, trace_generations_are_consecutive);
        assert!(!report.exit_criterion_met());
    }

    #[test]
    fn benign_no_diff_observation_is_recorded_and_tolerated_between_required_transitions() {
        let mut report = complete_report();
        let output_layouts = report.observations[0].output_layouts.clone();
        report.observations.insert(
            1,
            TopologyObservation {
                udev_kind: "change",
                udev_sequence_number: 101,
                device: 200,
                path: "/dev/dri/card1".into(),
                connector_probe: Some(ConnectorProbe::Forced),
                connector_query_count: Some(1),
                drm_master_state: Some(DrmMasterState::RetainedImplicit),
                card_close: None,
                output_layouts,
                rejected_connectors: Vec::new(),
                diffs: Vec::new(),
                commands: Vec::new(),
            },
        );

        assert!(report.udev_registered);
        assert!(report.errors.is_empty());
        assert!(report.udev_sequence_numbers_strictly_increasing());
        assert!(report.filtered_sequence_gap_observed());
        assert!(report.exit_criterion_met());
        assert!(!report.all_observations_diff_producing());
        assert_eq!(report.no_diff_observation_count(), 1);
        assert!(report.success());
        let listing = report.to_strict_data();
        assert!(listing.contains("\"success\": true"));
        assert!(listing.contains("\"all_observations_diff_producing\": false"));
        assert!(listing.contains("\"no_diff_observation_count\": 1"));
        assert!(listing.contains("\"coalesced_transitions_excluded\": false"));
        assert!(listing.contains("\"udev_sequence_numbers_strictly_increasing\": true"));
        assert!(listing.contains("\"filtered_sequence_gap_observed\": true"));
        assert!(listing.contains(
            "\"reconciliation\": \
                 \"no-change-after-forced-detection-coalescing-not-excluded\""
        ));
        assert!(listing.contains("\"udev_sequence_number\": 101"));
    }

    #[test]
    fn unrelated_non_master_cached_observation_does_not_weaken_target_probe_proof() {
        let mut report = complete_report();
        let output_layouts = report.observations[0].output_layouts.clone();
        report.observations.insert(
            1,
            TopologyObservation {
                udev_kind: "change",
                udev_sequence_number: 101,
                device: 100,
                path: "/dev/dri/card0".into(),
                connector_probe: Some(ConnectorProbe::Cached),
                connector_query_count: Some(1),
                drm_master_state: Some(DrmMasterState::NotMaster),
                card_close: None,
                output_layouts,
                rejected_connectors: Vec::new(),
                diffs: Vec::new(),
                commands: Vec::new(),
            },
        );

        assert!(trace_uses_required_connector_probe_policy(
            &report.observations
        ));
        assert!(report.exit_criterion_met());
    }

    #[test]
    fn missing_disconnect_transition_fails_despite_other_required_transitions() {
        let mut report = complete_report();
        report.observations.remove(1);

        assert!(report.udev_sequence_numbers_strictly_increasing());
        assert!(report.all_observations_diff_producing());
        assert!(!trace_has_exactly_four_diff_command_pairs(
            &report.observations
        ));
        assert!(!report.exit_criterion_met());
        assert!(!report.success());
    }

    #[test]
    fn watch_success_rejects_non_monotonic_udev_sequence_as_the_sole_falsifier() {
        let mut report = complete_report();
        report.observations[2].udev_sequence_number = 101;

        assert!(report.udev_registered);
        assert!(report.monitor_started_before_snapshot);
        assert!(report.errors.is_empty());
        assert!(report.all_observations_diff_producing());
        assert!(report.exit_criterion_met());
        assert!(!report.udev_sequence_numbers_strictly_increasing());
        assert!(!report.success());
    }

    #[test]
    fn watch_success_rejects_unregistered_udev_as_the_sole_falsifier() {
        let mut report = complete_report();
        report.udev_registered = false;

        assert!(report.monitor_started_before_snapshot);
        assert!(report.errors.is_empty());
        assert!(report.all_observations_diff_producing());
        assert!(report.udev_sequence_numbers_strictly_increasing());
        assert!(report.exit_criterion_met());
        assert!(!report.success());
    }

    #[test]
    fn watch_success_rejects_snapshot_before_monitor_as_the_sole_falsifier() {
        let mut report = complete_report();
        report.monitor_started_before_snapshot = false;

        assert!(report.udev_registered);
        assert!(report.errors.is_empty());
        assert!(report.all_observations_diff_producing());
        assert!(report.udev_sequence_numbers_strictly_increasing());
        assert!(report.exit_criterion_met());
        assert!(!report.success());
    }

    #[test]
    fn watch_success_rejects_delivery_error_as_the_sole_falsifier() {
        let mut report = complete_report();
        report.errors.push("forced delivery error".into());

        assert!(report.udev_registered);
        assert!(report.monitor_started_before_snapshot);
        assert!(report.all_observations_diff_producing());
        assert!(report.udev_sequence_numbers_strictly_increasing());
        assert!(report.exit_criterion_met());
        assert!(!report.success());
    }

    #[test]
    fn unresolved_initial_connector_is_the_thirteenth_and_sole_exit_falsifier() {
        let mut report = complete_report();
        let rejection = rejected_connector(300, "Virtual-9");
        report
            .initial_cards
            .push((300, PathBuf::from("/dev/dri/card2")));
        report.initial_rejected_connectors = vec![rejection.clone()];
        for observation in &mut report.observations {
            observation.rejected_connectors = vec![rejection.clone()];
        }

        assert!(report.udev_registered);
        assert!(report.monitor_started_before_snapshot);
        assert!(report.errors.is_empty());
        assert!(report.udev_sequence_numbers_strictly_increasing());
        assert!(correct_exit_trace(&report.observations));
        assert!(!report.all_connected_connectors_resolved());
        assert!(!report.exit_criterion_met());
        assert!(!report.success());

        let listing = report.to_strict_data();
        assert!(listing.contains("\"schema_version\": 5"));
        assert!(listing.contains("\"success\": false"));
        assert!(listing.contains("\"all_connected_connectors_resolved\": false"));
        assert!(listing.contains("\"initial_rejected_connectors\": ["));
        assert!(listing.contains("\"rejected_connectors\": ["));
        assert!(listing.contains("\"dev_t\": 300"));
        assert!(listing.contains("\"connector_identity\": \"Virtual-9\""));
        assert!(listing.contains("\"candidate_mode_count\": 1"));
        assert!(listing.contains("\"rejected_mode_count\": 1"));
        assert!(listing.contains("\"highest_ranked_candidate\": {"));
        assert!(listing.contains("\"reason_code\": \"display-selection-rejected\""));
        assert!(listing.contains("\"reason\": \"the selected DRM mode is interlaced\""));
        let status = std::process::Command::new("/opt/cosmix/bin/mix")
            .args([
                "-c",
                "$data = data_parse(env(\"COSMIX_KMS_WATCH_TEST_DATA\")); \
                 if type($data) != \"map\" then die(\"KMS watch report is not a map\") end",
            ])
            .env("COSMIX_KMS_WATCH_TEST_DATA", listing)
            .status()
            .expect("parse the degraded strict-data report");
        assert!(status.success());
    }

    #[test]
    fn recovered_initial_rejection_does_not_permanently_degrade_readiness() {
        let mut report = complete_report();
        report.initial_rejected_connectors = vec![rejected_connector(300, "Virtual-9")];

        assert!(
            report
                .observations
                .iter()
                .all(|observation| observation.rejected_connectors.is_empty())
        );
        assert!(report.all_connected_connectors_resolved());
        assert!(report.exit_criterion_met());
        assert!(report.success());
    }

    #[test]
    fn udev_monitor_starts_before_initial_enumeration() {
        let order = std::cell::RefCell::new(Vec::new());
        let _ = initialise_udev_with(
            || {
                order.borrow_mut().push("monitor");
                Ok::<_, ()>(())
            },
            || {
                order.borrow_mut().push("enumeration");
                Ok::<_, ()>(Vec::new())
            },
        )
        .expect("injected initialisation");

        assert_eq!(*order.borrow(), ["monitor", "enumeration"]);
    }

    #[test]
    fn live_udev_filter_rejects_render_node_as_the_sole_falsifier() {
        let mut facts = requested_card_facts(EventType::Add);
        facts.sysname = "renderD128".into();

        assert_eq!(facts.seat, OsStr::new("seat0"));
        assert!(!is_requested_primary_card(&facts, "seat0"));
    }

    #[test]
    fn live_udev_filter_rejects_other_seat_as_the_sole_falsifier() {
        let mut facts = requested_card_facts(EventType::Add);
        facts.seat = "seat1".into();

        assert!(is_primary_card_sysname(&facts.sysname));
        assert!(!is_requested_primary_card(&facts, "seat0"));
    }

    #[test]
    fn change_for_card_missing_from_initial_snapshot_synthesises_add() {
        let facts = requested_card_facts(EventType::Change);

        assert!(matches!(
            classify_card_event(&facts, "seat0", None),
            Some(CardEvent::Added {
                device: 200,
                ref path,
                sequence_number: 100,
            }) if path == Path::new("/dev/dri/card1")
        ));
    }

    #[test]
    fn queued_add_already_present_in_snapshot_is_suppressed() {
        let facts = requested_card_facts(EventType::Add);

        assert!(classify_card_event(&facts, "seat0", Some(Path::new("/dev/dri/card1"))).is_none());
    }

    #[test]
    fn unscannable_initial_card_does_not_block_other_card_observation() {
        let cards = [
            (100, PathBuf::from("/dev/dri/card0")),
            (200, PathBuf::from("/dev/dri/card1")),
        ];
        let mut tracker = TopologyTracker::default();
        tracker
            .prime(&cards, &mut |device, path, connector_probe| {
                if device == 100 {
                    Err("forced card0 failure".into())
                } else {
                    Ok(ConnectorScan::from_scanned_connectors(
                        device,
                        path.to_path_buf(),
                        connector_probe,
                        DrmMasterState::RetainedImplicit,
                        [connector(ConnectorStatus::Disconnected, true)],
                    ))
                }
            })
            .expect("a per-card scan failure is not a watcher failure");

        let observation = tracker
            .handle(
                CardEvent::Changed {
                    device: 200,
                    sequence_number: 200,
                },
                &mut |device, path, connector_probe| {
                    Ok(ConnectorScan::from_scanned_connectors(
                        device,
                        path.to_path_buf(),
                        connector_probe,
                        DrmMasterState::RetainedImplicit,
                        [connector(ConnectorStatus::Connected, true)],
                    ))
                },
            )
            .expect("the scannable card remains observable");

        assert!(matches!(
            observation.diffs.as_slice(),
            [ConnectorDiff::Changed { .. }]
        ));
    }

    #[test]
    fn initial_scan_failure_is_recorded_against_its_card() {
        let mut tracker = TopologyTracker::default();
        let summary = tracker
            .prime(&[(100, PathBuf::from("/dev/dri/card0"))], &mut |_, _, _| {
                Err("forced permission failure".into())
            })
            .expect("a per-card scan failure is reportable");

        assert_eq!(
            summary.errors,
            [
                "initial scan failed for DRM card /dev/dri/card0 (dev_t 100): forced permission failure"
            ]
        );
    }

    #[test]
    fn per_card_initial_scan_error_still_allows_udev_registration() {
        let mut report = complete_report();
        let summary = InitialScanSummary {
            errors: vec!["forced card scan failure".into()],
            ..InitialScanSummary::default()
        };

        assert!(incorporate_initial_scan(&mut report, Ok(summary)));
    }

    #[test]
    fn per_card_initial_scan_error_is_preserved_in_the_report() {
        let mut report = complete_report();
        let summary = InitialScanSummary {
            errors: vec!["forced card scan failure".into()],
            ..InitialScanSummary::default()
        };
        let _registration_allowed = incorporate_initial_scan(&mut report, Ok(summary));

        assert_eq!(report.errors, ["forced card scan failure"]);
    }

    #[test]
    fn injected_scan_device_identity_is_enforced_independently() {
        let scan = ConnectorScan::empty(201, "/dev/dri/card1".into());
        assert!(
            validate_scan_identity(&scan, 200, Path::new("/dev/dri/card1"))
                .expect_err("dev_t mismatch")
                .contains("dev_t 201, expected 200")
        );
    }

    #[test]
    fn injected_scan_path_identity_is_enforced_independently() {
        let scan = ConnectorScan::empty(200, "/dev/dri/card2".into());
        assert!(
            validate_scan_identity(&scan, 200, Path::new("/dev/dri/card1"))
                .expect_err("path mismatch")
                .contains("card2")
        );
    }

    #[test]
    fn injected_connector_device_identity_is_enforced_independently() {
        let scan = ConnectorScan::from_connectors(
            200,
            "/dev/dri/card1".into(),
            ConnectorProbe::Cached,
            DrmMasterState::NotMaster,
            [ConnectorInfo {
                key: OutputKey {
                    device: 201,
                    connector_name: "Virtual-1".into(),
                },
                ..connector(ConnectorStatus::Connected, true)
            }],
        );
        assert!(
            validate_scan_identity(&scan, 200, Path::new("/dev/dri/card1"))
                .expect_err("connector dev_t mismatch")
                .contains("belongs to dev_t 201")
        );
    }

    #[test]
    fn watch_report_is_mix_strict_data() {
        let report = KmsWatchReport {
            requested_seat: "seat0".into(),
            duration_seconds: 1,
            monitor_started_before_snapshot: true,
            udev_registered: true,
            initial_cards: vec![(57856, "/dev/dri/card0".into())],
            initial_drm_master_states: BTreeMap::from([(57856, DrmMasterState::NotMaster)]),
            initial_rejected_connectors: Vec::new(),
            watch_end_card_closes: BTreeMap::from([(57856, CardCloseOutcome::ClosedNonMaster)]),
            observations: Vec::new(),
            errors: Vec::new(),
        };
        let listing = report.to_strict_data();
        assert!(listing.contains("\"schema_version\": 5"));
        assert!(listing.contains("\"all_connected_connectors_resolved\": true"));
        assert!(listing.contains("\"initial_rejected_connectors\": ["));
        assert!(listing.contains("\"implicit_drm_master_possible_on_open\": true"));
        assert!(listing.contains("\"drm_master_acquire_ioctl_attempted\": false"));
        assert!(listing.contains("\"drm_master_state_checked_without_acquire_or_drop\": true"));
        assert!(listing.contains(
            "\"connector_probe_policy\": \
                 \"force-when-retained-implicit-master-otherwise-cached\""
        ));
        assert!(listing.contains("\"forced_probe_on_non_master_card\": false"));
        assert!(listing.contains("\"cached_metadata_may_be_stale_when_not_master\": true"));
        assert!(listing.contains(
            "\"non_master_observation_limit\": \
             \"cached-only-rung-c-topology-diff-exit-not-met\""
        ));
        assert!(listing.contains("\"rung_c_exit_requires_forced_event_probes\": true"));
        assert!(listing.contains("\"forced_probe_may_overrun_watch_duration\": true"));
        assert!(listing.contains("\"protocol_runtime_active_during_watch\": false"));
        assert!(listing.contains("\"monitor_started_before_snapshot\": true"));
        assert!(listing.contains("\"no_diff_cause_distinguishable\": false"));
        assert!(listing.contains("\"coalesced_transitions_excluded\": false"));
        assert!(listing.contains("\"event_transition_causation_proven\": false"));
        assert!(listing.contains("\"udev_sequence_numbers_recorded\": true"));
        assert!(listing.contains("\"filtered_sequence_gaps_prove_event_loss\": false"));
        assert!(
            listing.contains("\"drm_master_state\": \"not-master-existing-owner-or-ineligible\"")
        );
        assert!(listing.contains("\"outcome\": \"closed-non-master-fd\""));
        let status = std::process::Command::new("/opt/cosmix/bin/mix")
            .args([
                "-c",
                "$data = data_parse(env(\"COSMIX_KMS_WATCH_TEST_DATA\")); \
                 if type($data) != \"map\" then die(\"KMS watch report is not a map\") end",
            ])
            .env("COSMIX_KMS_WATCH_TEST_DATA", listing)
            .status()
            .expect("run the authoritative Mix strict-data parser");
        assert!(status.success());
    }

    fn complete_report() -> KmsWatchReport {
        let path = PathBuf::from("/dev/dri/card1");
        let mut tracker = TopologyTracker::default();
        let mut primary = connector(ConnectorStatus::Connected, true);
        primary.key.device = 100;
        primary.key.connector_name = "HDMI-A-1".into();
        primary.connector_id = 10;
        primary.name = "HDMI-A-1".into();
        primary.interface = "HDMI-A".into();
        primary.modes[0].width = 3840;
        primary.modes[0].height = 2160;
        tracker
            .prime(
                &[(100, PathBuf::from("/dev/dri/card0"))],
                &mut |device, path, requested_probe| {
                    assert_eq!(requested_probe, ConnectorProbe::Forced);
                    Ok(ConnectorScan::from_connectors(
                        device,
                        path.to_path_buf(),
                        ConnectorProbe::Cached,
                        DrmMasterState::NotMaster,
                        [primary.clone()],
                    ))
                },
            )
            .expect("existing primary topology");
        let added = tracker
            .handle(
                CardEvent::Added {
                    device: 200,
                    path,
                    sequence_number: 100,
                },
                &mut |_, _, _| Ok(scan(Some(connector(ConnectorStatus::Connected, true)))),
            )
            .expect("complete trace add");
        let disconnected = tracker
            .handle(
                CardEvent::Changed {
                    device: 200,
                    sequence_number: 102,
                },
                &mut |_, _, _| Ok(scan(Some(connector(ConnectorStatus::Disconnected, true)))),
            )
            .expect("complete trace disconnect");
        let reconnected = tracker
            .handle(
                CardEvent::Changed {
                    device: 200,
                    sequence_number: 104,
                },
                &mut |_, _, _| Ok(scan(Some(connector(ConnectorStatus::Connected, false)))),
            )
            .expect("complete trace reconnect");
        let mut removed = tracker
            .handle(
                CardEvent::Removed {
                    device: 200,
                    sequence_number: 106,
                },
                &mut |_, _, _| unreachable!("remove does not scan"),
            )
            .expect("complete trace remove");
        removed.card_close = Some(CardCloseOutcome::ClosedRetainedImplicitMaster);
        let KmsRenderCommand::AddOutput { output, .. } = &added.commands[0] else {
            unreachable!("complete trace add command");
        };
        assert_eq!(output.logical_rect.x, 3840);
        KmsWatchReport {
            requested_seat: "seat0".into(),
            duration_seconds: 5,
            monitor_started_before_snapshot: true,
            udev_registered: true,
            initial_cards: vec![(100, "/dev/dri/card0".into())],
            initial_drm_master_states: BTreeMap::from([(100, DrmMasterState::NotMaster)]),
            initial_rejected_connectors: Vec::new(),
            watch_end_card_closes: BTreeMap::from([(100, CardCloseOutcome::ClosedNonMaster)]),
            observations: vec![added, disconnected, reconnected, removed],
            errors: Vec::new(),
        }
    }

    fn assert_all_exit_guards_except(
        trace: &[TopologyObservation],
        expected_falsifier: fn(&[TopologyObservation]) -> bool,
    ) {
        assert!(!expected_falsifier(trace), "named guard must be false");
        let guard_results = [
            trace_has_exactly_four_diff_command_pairs(trace),
            trace_uses_required_connector_probe_policy(trace),
            trace_has_expected_card_event_order(trace),
            trace_has_expected_diff_transitions(trace),
            trace_keeps_card_identity(trace),
            trace_keeps_connector_identity(trace),
            trace_connectors_belong_to_observed_card(trace),
            trace_diff_chain_is_contiguous(trace),
            trace_commands_match_transitions(trace),
            trace_command_keys_match(trace),
            trace_command_mode_and_layout_match(trace),
            trace_generations_are_consecutive(trace),
        ];
        assert_eq!(
            guard_results.iter().filter(|result| !**result).count(),
            1,
            "the named guard must be the sole falsifier: {guard_results:?}"
        );
    }

    fn changed_previous_mut(observation: &mut TopologyObservation) -> &mut ConnectorInfo {
        match &mut observation.diffs[0] {
            ConnectorDiff::Changed { previous, .. } => previous,
            _ => panic!("fixture diff is not Changed"),
        }
    }

    fn changed_next_mut(observation: &mut TopologyObservation) -> &mut ConnectorInfo {
        match &mut observation.diffs[0] {
            ConnectorDiff::Changed { connector, .. } => connector,
            _ => panic!("fixture diff is not Changed"),
        }
    }

    fn set_diff_connector_name(diff: &mut ConnectorDiff, name: &str) {
        match diff {
            ConnectorDiff::Added { connector } | ConnectorDiff::Removed { connector } => {
                connector.key.connector_name = name.into();
            }
            ConnectorDiff::Changed {
                previous,
                connector,
            } => {
                previous.key.connector_name = name.into();
                connector.key.connector_name = name.into();
            }
        }
    }

    fn set_diff_connector_device(diff: &mut ConnectorDiff, device: DeviceId) {
        match diff {
            ConnectorDiff::Added { connector } | ConnectorDiff::Removed { connector } => {
                connector.key.device = device;
            }
            ConnectorDiff::Changed {
                previous,
                connector,
            } => {
                previous.key.device = device;
                connector.key.device = device;
            }
        }
    }

    fn set_layout_connector_name(
        layouts: &mut BTreeMap<OutputKey, LogicalRect>,
        connector_name: &str,
    ) {
        let Some((key, rect)) = layouts
            .iter()
            .find(|(key, _)| key.device == 200)
            .map(|(key, rect)| (key.clone(), *rect))
        else {
            return;
        };
        layouts.remove(&key);
        layouts.insert(
            OutputKey {
                connector_name: connector_name.into(),
                ..key
            },
            rect,
        );
    }

    fn set_layout_connector_device(
        layouts: &mut BTreeMap<OutputKey, LogicalRect>,
        device: DeviceId,
    ) {
        let Some((key, rect)) = layouts
            .iter()
            .find(|(key, _)| key.device == 200)
            .map(|(key, rect)| (key.clone(), *rect))
        else {
            return;
        };
        layouts.remove(&key);
        layouts.insert(OutputKey { device, ..key }, rect);
    }

    fn set_command_connector_name(command: &mut KmsRenderCommand, name: &str) {
        match command {
            KmsRenderCommand::AddOutput { output, .. }
            | KmsRenderCommand::ChangeOutput { output, .. } => {
                output.key.connector_name = name.into();
            }
            KmsRenderCommand::RemoveOutput { key, .. } => {
                key.connector_name = name.into();
            }
            KmsRenderCommand::Suspend { .. } | KmsRenderCommand::Resume { .. } => {
                panic!("fixture command has no connector")
            }
        }
    }

    fn set_command_connector_device(command: &mut KmsRenderCommand, device: DeviceId) {
        match command {
            KmsRenderCommand::AddOutput { output, .. }
            | KmsRenderCommand::ChangeOutput { output, .. } => {
                output.key.device = device;
            }
            KmsRenderCommand::RemoveOutput { key, .. } => {
                key.device = device;
            }
            KmsRenderCommand::Suspend { .. } | KmsRenderCommand::Resume { .. } => {
                panic!("fixture command has no connector")
            }
        }
    }

    fn set_command_generation(command: &mut KmsRenderCommand, value: u64) {
        match command {
            KmsRenderCommand::Suspend { generation }
            | KmsRenderCommand::Resume { generation }
            | KmsRenderCommand::AddOutput { generation, .. }
            | KmsRenderCommand::ChangeOutput { generation, .. }
            | KmsRenderCommand::RemoveOutput { generation, .. } => *generation = value,
        }
    }

    fn requested_card_facts(event_type: EventType) -> UdevCardFacts {
        UdevCardFacts {
            event_type,
            sequence_number: 100,
            device: Some(200),
            path: Some("/dev/dri/card1".into()),
            sysname: "card1".into(),
            seat: "seat0".into(),
        }
    }
}
