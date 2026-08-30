//! Pure KMS topology state.
//!
//! This module deliberately knows nothing about DRM, Vulkan, file descriptors,
//! or ioctls. The protocol thread supplies connector descriptions and an
//! already-admitted atomic output selection; the reducer emits lossless
//! commands for the render thread.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

/// One output scale in the 120ths used by the fractional-scale protocol.
///
/// Keeping this exact through admission prevents the physical mode and the
/// logical output extent from drifting through floating-point rounding. A
/// floating-point value is derived only at the Smithay boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct OutputScale120(u32);

impl OutputScale120 {
    pub(crate) const ONE: Self = Self(120);

    pub(crate) const fn new(scale120: u32) -> Option<Self> {
        if scale120 == 0 {
            None
        } else {
            Some(Self(scale120))
        }
    }

    pub(crate) const fn get(self) -> u32 {
        self.0
    }

    pub(crate) fn as_f64(self) -> f64 {
        f64::from(self.0) / 120.0
    }

    pub(crate) fn logical_extent(self, physical: (u32, u32)) -> Option<(u32, u32)> {
        let scale = u64::from(self.0);
        let logical_dimension = |dimension: u32| {
            let numerator = u64::from(dimension) * 120;
            numerator
                .is_multiple_of(scale)
                .then(|| u32::try_from(numerator / scale).ok())
                .flatten()
        };
        Some((
            logical_dimension(physical.0)?,
            logical_dimension(physical.1)?,
        ))
    }
}

impl Default for OutputScale120 {
    fn default() -> Self {
        Self::ONE
    }
}

impl fmt::Display for OutputScale120 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_multiple_of(120) {
            return write!(formatter, "{}", self.0 / 120);
        }
        if self.0.is_multiple_of(3) {
            let thousandths = u64::from(self.0) / 3 * 25;
            let whole = thousandths / 1_000;
            let fraction = format!("{:03}", thousandths % 1_000);
            return write!(formatter, "{whole}.{}", fraction.trim_end_matches('0'));
        }
        write!(formatter, "{}/120", self.0)
    }
}

/// Linux `dev_t`, represented losslessly by the type returned from
/// `MetadataExt::rdev` on supported targets.
pub(crate) type DeviceId = u64;

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct OutputKey {
    pub(crate) device: DeviceId,
    pub(crate) connector_name: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ConnectorMode {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) refresh_millihz: u32,
    pub(crate) preferred: bool,
    pub(crate) clock_khz: u32,
    pub(crate) hsync: (u16, u16, u16),
    pub(crate) vsync: (u16, u16, u16),
    pub(crate) hskew: u16,
    pub(crate) vscan: u16,
    pub(crate) flags: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ConnectorDescription {
    pub(crate) key: OutputKey,
    /// Current DRM connector object ID used by `SurfaceTargetUnsafe::Drm`.
    ///
    /// This is deliberately not part of [`OutputKey`]: the kernel may recycle
    /// object IDs while the sysfs connector name remains the stable topology
    /// identity.
    pub(crate) connector_id: u32,
    pub(crate) modes: Vec<ConnectorMode>,
}

/// Atomic admission made on the coordinator/pump side and consumed as inert
/// topology data by the protocol thread.
#[cfg(any(all(feature = "kms-live", not(test)), test))]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PreselectedAtomicOutput {
    pub(crate) key: OutputKey,
    pub(crate) connector_mode: ConnectorMode,
    pub(crate) selection: Result<AtomicOutputSelection, String>,
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct KmsTopologySnapshot {
    pub(crate) connectors: Vec<ConnectorDescription>,
    pub(crate) selections: Vec<PreselectedAtomicOutput>,
    pub(crate) output_scale: OutputScale120,
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum KmsTopologyLifecycleEvent {
    Initial(KmsTopologySnapshot),
    Pause,
    Resume(KmsTopologySnapshot),
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum PresentationBackend {
    #[default]
    Atomic,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct AtomicOutputSelection {
    pub(crate) connector_id: u32,
    pub(crate) crtc_id: u32,
    pub(crate) primary_plane_id: u32,
    pub(crate) mode: ConnectorMode,
    pub(crate) format: u32,
    pub(crate) modifier: u64,
}

const DRM_MODE_FLAG_INTERLACE: u32 = 1 << 4;
const DRM_MODE_FLAG_DBLSCAN: u32 = 1 << 5;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ConnectorModeTuple {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) refresh_millihz: u32,
}

impl From<ConnectorMode> for ConnectorModeTuple {
    fn from(mode: ConnectorMode) -> Self {
        Self {
            width: mode.width,
            height: mode.height,
            refresh_millihz: mode.refresh_millihz,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ConnectorRejection {
    pub(crate) key: OutputKey,
    pub(crate) candidate_mode_count: usize,
    pub(crate) rejected_mode_count: usize,
    pub(crate) highest_ranked_candidate: Option<ConnectorModeTuple>,
    pub(crate) reason_code: &'static str,
    pub(crate) reason: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct LogicalRect {
    pub(crate) x: i32,
    pub(crate) y: i32,
    pub(crate) width: i32,
    pub(crate) height: i32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SelectedOutput {
    pub(crate) key: OutputKey,
    pub(crate) connector_id: u32,
    pub(crate) connector_mode: ConnectorMode,
    pub(crate) display: AtomicOutputSelection,
    pub(crate) output_scale: OutputScale120,
    pub(crate) logical_rect: LogicalRect,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum KmsRenderOperation {
    Worker,
    Suspend,
    Resume,
    AddOutput,
    ChangeOutput,
    RemoveOutput,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum KmsRenderCommand {
    Suspend {
        generation: u64,
    },
    Resume {
        generation: u64,
    },
    AddOutput {
        generation: u64,
        output: SelectedOutput,
    },
    ChangeOutput {
        generation: u64,
        output: SelectedOutput,
    },
    RemoveOutput {
        generation: u64,
        key: OutputKey,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum KmsRenderReply {
    Suspended {
        generation: u64,
    },
    OutputReady {
        generation: u64,
        key: OutputKey,
    },
    OutputFailed {
        generation: u64,
        key: OutputKey,
        reason: String,
    },
    OutputRemoved {
        generation: u64,
        key: OutputKey,
    },
    WorkerFailed {
        operation: KmsRenderOperation,
        generation: u64,
        key: Option<OutputKey>,
        code: &'static str,
        reason: String,
    },
    FrameSubmitted {
        generation: u64,
        key: OutputKey,
    },
}

pub(crate) struct SuspendedReply {
    pub(crate) generation: u64,
}

impl From<u64> for SuspendedReply {
    fn from(generation: u64) -> Self {
        Self { generation }
    }
}

impl From<SuspendedReply> for KmsRenderReply {
    fn from(reply: SuspendedReply) -> Self {
        Self::Suspended {
            generation: reply.generation,
        }
    }
}

pub(crate) struct OutputReadyReply {
    pub(crate) generation: u64,
    pub(crate) key: OutputKey,
}

impl From<(u64, OutputKey)> for OutputReadyReply {
    fn from((generation, key): (u64, OutputKey)) -> Self {
        Self { generation, key }
    }
}

impl From<OutputReadyReply> for KmsRenderReply {
    fn from(reply: OutputReadyReply) -> Self {
        Self::OutputReady {
            generation: reply.generation,
            key: reply.key,
        }
    }
}

pub(crate) struct OutputFailedReply {
    pub(crate) generation: u64,
    pub(crate) key: OutputKey,
    pub(crate) reason: String,
}

impl From<(u64, OutputKey, String)> for OutputFailedReply {
    fn from((generation, key, reason): (u64, OutputKey, String)) -> Self {
        Self {
            generation,
            key,
            reason,
        }
    }
}

impl From<OutputFailedReply> for KmsRenderReply {
    fn from(reply: OutputFailedReply) -> Self {
        Self::OutputFailed {
            generation: reply.generation,
            key: reply.key,
            reason: reply.reason,
        }
    }
}

pub(crate) struct OutputRemovedReply {
    pub(crate) generation: u64,
    pub(crate) key: OutputKey,
}

impl From<(u64, OutputKey)> for OutputRemovedReply {
    fn from((generation, key): (u64, OutputKey)) -> Self {
        Self { generation, key }
    }
}

impl From<OutputRemovedReply> for KmsRenderReply {
    fn from(reply: OutputRemovedReply) -> Self {
        Self::OutputRemoved {
            generation: reply.generation,
            key: reply.key,
        }
    }
}

pub(crate) struct FrameSubmittedReply {
    pub(crate) generation: u64,
    pub(crate) key: OutputKey,
}

impl From<(u64, OutputKey)> for FrameSubmittedReply {
    fn from((generation, key): (u64, OutputKey)) -> Self {
        Self { generation, key }
    }
}

impl From<FrameSubmittedReply> for KmsRenderReply {
    fn from(reply: FrameSubmittedReply) -> Self {
        Self::FrameSubmitted {
            generation: reply.generation,
            key: reply.key,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum KmsTopologyEvent {
    #[cfg_attr(not(any(feature = "kms-live", test)), allow(dead_code))]
    ConnectorScan(Vec<ConnectorDescription>),
    UdevChange(Vec<ConnectorDescription>),
    SessionPause,
    SessionResume(Vec<ConnectorDescription>),
    RenderReply(KmsRenderReply),
}

pub(crate) struct UdevConnectorScan(pub(crate) Vec<ConnectorDescription>);

impl From<Vec<ConnectorDescription>> for UdevConnectorScan {
    fn from(connectors: Vec<ConnectorDescription>) -> Self {
        Self(connectors)
    }
}

impl From<UdevConnectorScan> for KmsTopologyEvent {
    fn from(scan: UdevConnectorScan) -> Self {
        Self::UdevChange(scan.0)
    }
}

pub(crate) struct SessionPause;

impl From<()> for SessionPause {
    fn from((): ()) -> Self {
        Self
    }
}

impl From<SessionPause> for KmsTopologyEvent {
    fn from(_: SessionPause) -> Self {
        Self::SessionPause
    }
}

pub(crate) struct SessionResume(pub(crate) Vec<ConnectorDescription>);

impl From<Vec<ConnectorDescription>> for SessionResume {
    fn from(connectors: Vec<ConnectorDescription>) -> Self {
        Self(connectors)
    }
}

impl From<SessionResume> for KmsTopologyEvent {
    fn from(resume: SessionResume) -> Self {
        Self::SessionResume(resume.0)
    }
}

impl From<KmsRenderReply> for KmsTopologyEvent {
    fn from(reply: KmsRenderReply) -> Self {
        Self::RenderReply(reply)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OutputPhase {
    Adding,
    Changing,
    Ready,
    Removing,
    RemovalFailed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct OutputState {
    pub(crate) generation: u64,
    pub(crate) phase: OutputPhase,
    pub(crate) selected: SelectedOutput,
    pub(crate) frames_submitted: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct OutputFailure {
    pub(crate) generation: u64,
    pub(crate) reason: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum IgnoredRenderReplyReason {
    Inactive,
    UnknownOutput,
    SupersededGeneration { expected: u64 },
    UnexpectedPhase { phase: OutputPhase },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct IgnoredRenderReply {
    pub(crate) reply: KmsRenderReply,
    pub(crate) reason: IgnoredRenderReplyReason,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum KmsTopologyError {
    DuplicateConnector(OutputKey),
    #[cfg_attr(not(any(all(feature = "kms-live", not(test)), test)), allow(dead_code))]
    OutputScaleChanged {
        previous: OutputScale120,
        resumed: OutputScale120,
    },
    LogicalCoordinateOverflow,
    GenerationExhausted,
    RenderWorkerFailed {
        operation: KmsRenderOperation,
        generation: u64,
        key: Option<OutputKey>,
        code: &'static str,
        reason: String,
    },
}

impl fmt::Display for KmsTopologyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateConnector(key) => write!(
                formatter,
                "connector {}:{} appeared more than once in a full scan",
                key.device, key.connector_name
            ),
            Self::OutputScaleChanged { previous, resumed } => write!(
                formatter,
                "output scale changed across pause/resume from {previous} to {resumed}"
            ),
            Self::LogicalCoordinateOverflow => {
                formatter.write_str("combined output layout exceeds logical i32 coordinates")
            }
            Self::GenerationExhausted => formatter.write_str("KMS generation counter exhausted"),
            Self::RenderWorkerFailed {
                operation,
                generation,
                key,
                code,
                reason,
            } => write!(
                formatter,
                "KMS render worker failed during {operation:?} generation {generation} for {key:?}: {code}: {reason}"
            ),
        }
    }
}

impl std::error::Error for KmsTopologyError {}

#[derive(Clone, Debug)]
pub(crate) struct KmsTopology {
    active: bool,
    suspend_confirmed: bool,
    generation: u64,
    session_generation: u64,
    outputs: BTreeMap<OutputKey, OutputState>,
    failures: BTreeMap<OutputKey, OutputFailure>,
    rejected_connectors: BTreeMap<OutputKey, ConnectorRejection>,
    ignored_render_replies: u64,
    last_ignored_render_reply: Option<IgnoredRenderReply>,
    output_scale: OutputScale120,
}

impl Default for KmsTopology {
    fn default() -> Self {
        Self {
            active: true,
            suspend_confirmed: false,
            generation: 0,
            session_generation: 0,
            outputs: BTreeMap::new(),
            failures: BTreeMap::new(),
            rejected_connectors: BTreeMap::new(),
            ignored_render_replies: 0,
            last_ignored_render_reply: None,
            output_scale: OutputScale120::ONE,
        }
    }
}

impl KmsTopology {
    #[cfg(test)]
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    #[cfg(test)]
    pub(crate) fn is_active(&self) -> bool {
        self.active
    }

    #[cfg(test)]
    pub(crate) fn suspend_confirmed(&self) -> bool {
        self.suspend_confirmed
    }

    #[cfg(test)]
    pub(crate) fn output(&self, key: OutputKey) -> Option<&OutputState> {
        self.outputs.get(&key)
    }

    #[cfg(test)]
    pub(crate) fn failure(&self, key: OutputKey) -> Option<&OutputFailure> {
        self.failures.get(&key)
    }

    #[cfg(test)]
    pub(crate) fn ignored_render_replies(&self) -> u64 {
        self.ignored_render_replies
    }

    #[cfg(test)]
    pub(crate) fn last_ignored_render_reply(&self) -> Option<&IgnoredRenderReply> {
        self.last_ignored_render_reply.as_ref()
    }

    pub(crate) fn output_layouts(&self) -> BTreeMap<OutputKey, LogicalRect> {
        self.outputs
            .iter()
            .filter(|(_, output)| {
                !matches!(
                    output.phase,
                    OutputPhase::Removing | OutputPhase::RemovalFailed
                )
            })
            .map(|(key, output)| (key.clone(), output.selected.logical_rect))
            .collect()
    }

    /// The one logical output exposed to Wayland clients by the live KMS path.
    ///
    /// The live atomic path currently admits one connector. Keep this selection
    /// here, beside the topology ordering that assigns its logical location,
    /// rather than teaching the protocol registry a second selection policy.
    /// Removing outputs are already logically absent and must not remain the
    /// client output merely because the render worker has not acknowledged its
    /// teardown yet.
    pub(crate) fn selected_client_output(&self) -> Option<&SelectedOutput> {
        self.outputs
            .values()
            .find(|output| {
                !matches!(
                    output.phase,
                    OutputPhase::Removing | OutputPhase::RemovalFailed
                )
            })
            .map(|output| &output.selected)
    }

    /// The admitted scale remains session-lifetime state even while DRM master
    /// and the render-output records are suspended.
    pub(crate) fn output_scale(&self) -> OutputScale120 {
        self.selected_client_output()
            .map_or(self.output_scale, |output| output.output_scale)
    }

    pub(crate) fn rejected_connectors(&self) -> Vec<ConnectorRejection> {
        self.rejected_connectors.values().cloned().collect()
    }

    /// The coordinate space the seat's pointer lives in, or `None` when no
    /// output has been admitted yet.
    ///
    /// This is the far edge of the admitted layout, not one output's mode: a
    /// two-output seat is one continuous pointer space, and confining the
    /// cursor to either output alone would make the other unreachable. It is
    /// deliberately separate from `KmsBackendData::output_size`, which is the
    /// bootstrap extent the compositor starts at and which no KMS event
    /// updates — transforming a real 1920x1080 device into that placeholder
    /// would put the pointer in the wrong place on every absolute event.
    pub(crate) fn seat_extent(&self) -> Option<(u32, u32)> {
        self.output_layouts()
            .values()
            .map(|rect| {
                let edge = |origin: i32, extent: i32| {
                    u32::try_from((i64::from(origin) + i64::from(extent)).max(0))
                        .unwrap_or(u32::MAX)
                };
                (edge(rect.x, rect.width), edge(rect.y, rect.height))
            })
            .reduce(|far, edge| (far.0.max(edge.0), far.1.max(edge.1)))
    }

    /// Every admitted output as a pointer region.
    ///
    /// The union of these is the seat's real shape; `seat_extent` is only its
    /// bounding box, and the two differ the moment two outputs have unequal
    /// heights. Empty until an output is admitted.
    pub(crate) fn seat_regions(&self) -> Vec<super::SeatRegion> {
        self.output_layouts()
            .values()
            .map(|rect| super::SeatRegion {
                x: f64::from(rect.x),
                y: f64::from(rect.y),
                width: f64::from(rect.width),
                height: f64::from(rect.height),
            })
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn logical_rect(&self, key: OutputKey) -> Option<LogicalRect> {
        self.output_layouts().get(&key).copied()
    }

    /// Apply one event transactionally.
    ///
    /// `select_display` is the only bridge to atomic admission. It receives
    /// the selected connector mode and must return the matching route and
    /// scanout format.
    pub(crate) fn reduce<F>(
        &mut self,
        event: KmsTopologyEvent,
        select_display: &mut F,
    ) -> Result<Vec<KmsRenderCommand>, KmsTopologyError>
    where
        F: FnMut(&ConnectorDescription, ConnectorMode) -> Result<AtomicOutputSelection, String>,
    {
        let mut next = self.clone();
        let commands = next.reduce_inner(event, select_display)?;
        *self = next;
        Ok(commands)
    }

    /// Reduce a lifecycle event using only selections supplied as data.
    /// Atomic discovery never runs on the protocol owner.
    #[cfg(any(all(feature = "kms-live", not(test)), test))]
    pub(crate) fn reduce_lifecycle(
        &mut self,
        event: KmsTopologyLifecycleEvent,
    ) -> Result<Vec<KmsRenderCommand>, KmsTopologyError> {
        let (event, selections, output_scale) = match event {
            KmsTopologyLifecycleEvent::Initial(snapshot) => (
                KmsTopologyEvent::ConnectorScan(snapshot.connectors),
                snapshot.selections,
                Some(snapshot.output_scale),
            ),
            KmsTopologyLifecycleEvent::Pause => (KmsTopologyEvent::SessionPause, Vec::new(), None),
            KmsTopologyLifecycleEvent::Resume(snapshot) => (
                KmsTopologyEvent::SessionResume(snapshot.connectors),
                snapshot.selections,
                Some(snapshot.output_scale),
            ),
        };
        if matches!(&event, KmsTopologyEvent::SessionResume(_))
            && output_scale.is_some_and(|resumed| resumed != self.output_scale)
        {
            return Err(KmsTopologyError::OutputScaleChanged {
                previous: self.output_scale,
                resumed: output_scale.expect("mismatched resume scale is present"),
            });
        }
        let mut next = self.clone();
        if let Some(output_scale) = output_scale {
            next.output_scale = output_scale;
        }
        let commands = next.reduce(event, &mut |connector, mode| {
            selections
                .iter()
                .find(|selection| {
                    selection.key == connector.key && selection.connector_mode == mode
                })
                .map(|selection| selection.selection.clone())
                .unwrap_or_else(|| {
                    Err(format!(
                        "no atomic selection data for {} mode {}x{}@{}mHz",
                        connector.key.connector_name, mode.width, mode.height, mode.refresh_millihz
                    ))
                })
        })?;
        *self = next;
        Ok(commands)
    }

    fn reduce_inner<F>(
        &mut self,
        event: KmsTopologyEvent,
        select_display: &mut F,
    ) -> Result<Vec<KmsRenderCommand>, KmsTopologyError>
    where
        F: FnMut(&ConnectorDescription, ConnectorMode) -> Result<AtomicOutputSelection, String>,
    {
        match event {
            KmsTopologyEvent::ConnectorScan(connectors)
            | KmsTopologyEvent::UdevChange(connectors) => {
                if self.active {
                    self.apply_scan(connectors, select_display)
                } else {
                    Ok(Vec::new())
                }
            }
            KmsTopologyEvent::SessionPause => self.pause(),
            KmsTopologyEvent::SessionResume(connectors) => self.resume(connectors, select_display),
            KmsTopologyEvent::RenderReply(reply) => self.apply_reply(reply),
        }
    }

    fn pause(&mut self) -> Result<Vec<KmsRenderCommand>, KmsTopologyError> {
        if !self.active {
            return Ok(Vec::new());
        }
        let generation = self.next_generation()?;
        self.active = false;
        self.suspend_confirmed = false;
        self.session_generation = generation;
        self.outputs.clear();
        self.failures.clear();
        self.rejected_connectors.clear();
        Ok(vec![KmsRenderCommand::Suspend { generation }])
    }

    fn resume<F>(
        &mut self,
        connectors: Vec<ConnectorDescription>,
        select_display: &mut F,
    ) -> Result<Vec<KmsRenderCommand>, KmsTopologyError>
    where
        F: FnMut(&ConnectorDescription, ConnectorMode) -> Result<AtomicOutputSelection, String>,
    {
        if self.active {
            return self.apply_scan(connectors, select_display);
        }

        let generation = self.next_generation()?;
        self.active = true;
        self.suspend_confirmed = false;
        self.session_generation = generation;
        self.outputs.clear();
        self.failures.clear();
        self.rejected_connectors.clear();

        let mut commands = vec![KmsRenderCommand::Resume { generation }];
        commands.extend(self.apply_scan(connectors, select_display)?);
        Ok(commands)
    }

    fn apply_scan<F>(
        &mut self,
        connectors: Vec<ConnectorDescription>,
        select_display: &mut F,
    ) -> Result<Vec<KmsRenderCommand>, KmsTopologyError>
    where
        F: FnMut(&ConnectorDescription, ConnectorMode) -> Result<AtomicOutputSelection, String>,
    {
        let resolution = resolve_connectors(connectors, self.output_scale, select_display)?;
        let desired = resolution.outputs;
        self.rejected_connectors = resolution.rejections;
        let desired_keys = desired.keys().cloned().collect::<BTreeSet<_>>();
        let removed = self
            .outputs
            .keys()
            .filter(|key| !desired_keys.contains(key))
            .cloned()
            .collect::<Vec<_>>();
        let mut commands = Vec::new();

        for key in removed {
            let output = self
                .outputs
                .get(&key)
                .expect("removed key came from the output map");
            if matches!(
                output.phase,
                OutputPhase::Removing | OutputPhase::RemovalFailed
            ) {
                continue;
            }
            let generation = self.next_generation()?;
            self.failures.remove(&key);
            let output = self
                .outputs
                .get_mut(&key)
                .expect("removed key remains in the output map");
            output.generation = generation;
            output.phase = OutputPhase::Removing;
            output.frames_submitted = 0;
            commands.push(KmsRenderCommand::RemoveOutput { generation, key });
        }

        self.failures.retain(|key, _| {
            self.outputs
                .get(key)
                .is_some_and(|output| output.phase == OutputPhase::RemovalFailed)
        });
        for (key, selected) in desired {
            let existing = self.outputs.get(&key);
            if existing.is_some_and(|output| {
                !matches!(
                    output.phase,
                    OutputPhase::Removing | OutputPhase::RemovalFailed
                ) && output.selected == selected
            }) {
                continue;
            }
            let changing = existing.is_some_and(|output| {
                !matches!(
                    output.phase,
                    OutputPhase::Removing | OutputPhase::RemovalFailed
                )
            });

            let generation = self.next_generation()?;
            let (phase, command) = if changing {
                (
                    OutputPhase::Changing,
                    KmsRenderCommand::ChangeOutput {
                        generation,
                        output: selected.clone(),
                    },
                )
            } else {
                (
                    OutputPhase::Adding,
                    KmsRenderCommand::AddOutput {
                        generation,
                        output: selected.clone(),
                    },
                )
            };
            self.failures.remove(&key);
            self.outputs.insert(
                key,
                OutputState {
                    generation,
                    phase,
                    selected,
                    frames_submitted: 0,
                },
            );
            commands.push(command);
        }

        Ok(commands)
    }

    fn apply_reply(
        &mut self,
        reply: KmsRenderReply,
    ) -> Result<Vec<KmsRenderCommand>, KmsTopologyError> {
        match reply.clone() {
            KmsRenderReply::Suspended { generation } => {
                if !self.active && generation == self.session_generation {
                    self.suspend_confirmed = true;
                } else {
                    let reason = if self.active {
                        IgnoredRenderReplyReason::Inactive
                    } else {
                        IgnoredRenderReplyReason::SupersededGeneration {
                            expected: self.session_generation,
                        }
                    };
                    self.record_ignored_reply(reply, reason);
                }
                Ok(Vec::new())
            }
            KmsRenderReply::OutputReady { generation, key } => {
                let Some(output) = self.outputs.get(&key) else {
                    self.record_ignored_reply(reply, IgnoredRenderReplyReason::UnknownOutput);
                    return Ok(Vec::new());
                };
                if !self.active {
                    self.record_ignored_reply(reply, IgnoredRenderReplyReason::Inactive);
                    return Ok(Vec::new());
                }
                if output.generation != generation {
                    self.record_ignored_reply(
                        reply,
                        IgnoredRenderReplyReason::SupersededGeneration {
                            expected: output.generation,
                        },
                    );
                    return Ok(Vec::new());
                }
                if !matches!(output.phase, OutputPhase::Adding | OutputPhase::Changing) {
                    self.record_ignored_reply(
                        reply,
                        IgnoredRenderReplyReason::UnexpectedPhase {
                            phase: output.phase,
                        },
                    );
                    return Ok(Vec::new());
                }
                self.outputs
                    .get_mut(&key)
                    .expect("validated output remains present")
                    .phase = OutputPhase::Ready;
                Ok(Vec::new())
            }
            KmsRenderReply::OutputFailed {
                generation,
                key,
                reason,
            } => self.output_failed(reply, key, generation, reason),
            KmsRenderReply::OutputRemoved { generation, key } => {
                let Some(output) = self.outputs.get(&key) else {
                    self.record_ignored_reply(reply, IgnoredRenderReplyReason::UnknownOutput);
                    return Ok(Vec::new());
                };
                if output.generation != generation {
                    self.record_ignored_reply(
                        reply,
                        IgnoredRenderReplyReason::SupersededGeneration {
                            expected: output.generation,
                        },
                    );
                    return Ok(Vec::new());
                }
                if output.phase != OutputPhase::Removing {
                    self.record_ignored_reply(
                        reply,
                        IgnoredRenderReplyReason::UnexpectedPhase {
                            phase: output.phase,
                        },
                    );
                    return Ok(Vec::new());
                }
                self.outputs.remove(&key);
                self.failures.remove(&key);
                Ok(Vec::new())
            }
            KmsRenderReply::WorkerFailed {
                operation,
                generation,
                key,
                code,
                reason,
            } => Err(KmsTopologyError::RenderWorkerFailed {
                operation,
                generation,
                key,
                code,
                reason,
            }),
            KmsRenderReply::FrameSubmitted { generation, key } => {
                let Some(output) = self.outputs.get(&key) else {
                    self.record_ignored_reply(reply, IgnoredRenderReplyReason::UnknownOutput);
                    return Ok(Vec::new());
                };
                if !self.active {
                    self.record_ignored_reply(reply, IgnoredRenderReplyReason::Inactive);
                    return Ok(Vec::new());
                }
                if output.generation != generation {
                    self.record_ignored_reply(
                        reply,
                        IgnoredRenderReplyReason::SupersededGeneration {
                            expected: output.generation,
                        },
                    );
                    return Ok(Vec::new());
                }
                if output.phase != OutputPhase::Ready {
                    self.record_ignored_reply(
                        reply,
                        IgnoredRenderReplyReason::UnexpectedPhase {
                            phase: output.phase,
                        },
                    );
                    return Ok(Vec::new());
                }
                let output = self
                    .outputs
                    .get_mut(&key)
                    .expect("validated output remains present");
                output.frames_submitted = output.frames_submitted.saturating_add(1);
                Ok(Vec::new())
            }
        }
    }

    fn output_failed(
        &mut self,
        reply: KmsRenderReply,
        key: OutputKey,
        generation: u64,
        reason: String,
    ) -> Result<Vec<KmsRenderCommand>, KmsTopologyError> {
        let Some(output) = self.outputs.get(&key) else {
            self.record_ignored_reply(reply, IgnoredRenderReplyReason::UnknownOutput);
            return Ok(Vec::new());
        };
        if output.generation != generation {
            self.record_ignored_reply(
                reply,
                IgnoredRenderReplyReason::SupersededGeneration {
                    expected: output.generation,
                },
            );
            return Ok(Vec::new());
        }

        if output.phase == OutputPhase::Removing {
            // Removal failure is terminal because udev need not emit again and retrying an
            // unknown platform state can duplicate teardown.
            self.outputs
                .get_mut(&key)
                .expect("validated removal remains present")
                .phase = OutputPhase::RemovalFailed;
            self.failures
                .insert(key, OutputFailure { generation, reason });
            return Ok(Vec::new());
        }

        self.outputs.remove(&key);
        self.failures
            .insert(key, OutputFailure { generation, reason });
        self.reflow_outputs()
    }

    fn reflow_outputs(&mut self) -> Result<Vec<KmsRenderCommand>, KmsTopologyError> {
        let mut x = 0_i32;
        let keys = self.outputs.keys().cloned().collect::<Vec<_>>();
        let mut changed = Vec::new();
        for key in keys {
            let output = self
                .outputs
                .get(&key)
                .expect("key came from the output map");
            if matches!(
                output.phase,
                OutputPhase::Removing | OutputPhase::RemovalFailed
            ) {
                continue;
            }
            let width = output.selected.logical_rect.width;
            let height = output.selected.logical_rect.height;
            let rect = LogicalRect {
                x,
                y: 0,
                width,
                height,
            };
            x = x
                .checked_add(width)
                .ok_or(KmsTopologyError::LogicalCoordinateOverflow)?;
            if rect != output.selected.logical_rect {
                changed.push((key, rect));
            }
        }

        let mut commands = Vec::new();
        for (key, rect) in changed {
            let generation = self.next_generation()?;
            let output = self
                .outputs
                .get_mut(&key)
                .expect("changed output remains present");
            output.generation = generation;
            output.phase = OutputPhase::Changing;
            output.frames_submitted = 0;
            output.selected.logical_rect = rect;
            commands.push(KmsRenderCommand::ChangeOutput {
                generation,
                output: output.selected.clone(),
            });
        }
        Ok(commands)
    }

    fn next_generation(&mut self) -> Result<u64, KmsTopologyError> {
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or(KmsTopologyError::GenerationExhausted)?;
        Ok(self.generation)
    }

    fn record_ignored_reply(&mut self, reply: KmsRenderReply, reason: IgnoredRenderReplyReason) {
        self.ignored_render_replies = self.ignored_render_replies.saturating_add(1);
        self.last_ignored_render_reply = Some(IgnoredRenderReply { reply, reason });
    }
}

#[derive(Debug, Eq, PartialEq)]
struct ConnectorResolution {
    outputs: BTreeMap<OutputKey, SelectedOutput>,
    rejections: BTreeMap<OutputKey, ConnectorRejection>,
}

struct RejectionReason {
    code: &'static str,
    detail: String,
}

fn resolve_connectors<F>(
    connectors: Vec<ConnectorDescription>,
    output_scale: OutputScale120,
    select_display: &mut F,
) -> Result<ConnectorResolution, KmsTopologyError>
where
    F: FnMut(&ConnectorDescription, ConnectorMode) -> Result<AtomicOutputSelection, String>,
{
    let mut resolved = BTreeMap::new();
    let mut rejections = BTreeMap::new();
    let mut seen = BTreeSet::new();
    for connector in connectors {
        let key = connector.key.clone();
        if !seen.insert(key.clone()) {
            return Err(KmsTopologyError::DuplicateConnector(key));
        }

        let ranked_modes = ranked_connector_modes(&connector);
        let candidate_mode_count = ranked_modes.len();
        let highest_ranked_candidate = ranked_modes.first().copied().map(ConnectorModeTuple::from);
        let mut rejected_mode_count = 0_usize;
        let mut highest_ranked_rejection = None;
        let mut selected = None;
        for mode in ranked_modes {
            if drm_mode_tuple_is_ambiguous(&connector, mode) {
                rejected_mode_count += 1;
                highest_ranked_rejection.get_or_insert_with(|| RejectionReason {
                    code: "ambiguous-drm-mode-tuple",
                    detail: format!(
                        "{}x{}@{} has more than one distinct DRM timing",
                        mode.width, mode.height, mode.refresh_millihz
                    ),
                });
                continue;
            }

            let Some(logical_extent) = output_scale.logical_extent((mode.width, mode.height))
            else {
                rejected_mode_count += 1;
                highest_ranked_rejection.get_or_insert_with(|| RejectionReason {
                    code: "non-integral-logical-extent",
                    detail: format!(
                        "{}x{} at scale {} does not produce an integral logical extent",
                        mode.width, mode.height, output_scale
                    ),
                });
                continue;
            };

            match select_display(&connector, mode) {
                Ok(display)
                    if display.connector_id == connector.connector_id
                        && same_drm_timing(display.mode, mode) =>
                {
                    selected = Some((mode, display, logical_extent));
                    break;
                }
                Ok(display) if display.connector_id != connector.connector_id => {
                    rejected_mode_count += 1;
                    highest_ranked_rejection.get_or_insert_with(|| RejectionReason {
                        code: "display-connector-id-mismatch",
                        detail: format!(
                            "atomic connector {} does not match scanned connector {}",
                            display.connector_id, connector.connector_id
                        ),
                    });
                }
                Ok(display) => {
                    rejected_mode_count += 1;
                    highest_ranked_rejection.get_or_insert_with(|| RejectionReason {
                        code: "display-mode-timing-mismatch",
                        detail: format!(
                            "atomic mode timing {:?} does not exactly match connector mode timing {:?}",
                            display.mode, mode
                        ),
                    });
                }
                Err(reason) => {
                    rejected_mode_count += 1;
                    highest_ranked_rejection.get_or_insert(RejectionReason {
                        code: "display-selection-rejected",
                        detail: reason,
                    });
                }
            }
        }

        let Some((mode, display, logical_extent)) = selected else {
            let rejection = highest_ranked_rejection.unwrap_or_else(|| RejectionReason {
                code: "no-candidate-modes",
                detail: "connector advertised no modes".into(),
            });
            tracing::warn!(
                device = key.device,
                connector = %key.connector_name,
                candidate_mode_count,
                rejected_mode_count,
                reason_code = rejection.code,
                reason = %rejection.detail,
                "skipping connector because no atomic output selection is compatible"
            );
            rejections.insert(
                key.clone(),
                ConnectorRejection {
                    key,
                    candidate_mode_count,
                    rejected_mode_count,
                    highest_ranked_candidate,
                    reason_code: rejection.code,
                    reason: rejection.detail,
                },
            );
            continue;
        };

        let width = i32::try_from(logical_extent.0)
            .map_err(|_| KmsTopologyError::LogicalCoordinateOverflow)?;
        let height = i32::try_from(logical_extent.1)
            .map_err(|_| KmsTopologyError::LogicalCoordinateOverflow)?;
        let selected_key = key.clone();
        resolved.insert(
            key,
            SelectedOutput {
                key: selected_key,
                connector_id: connector.connector_id,
                connector_mode: mode,
                display,
                output_scale,
                logical_rect: LogicalRect {
                    x: 0,
                    y: 0,
                    width,
                    height,
                },
            },
        );
    }

    let mut x = 0_i32;
    for output in resolved.values_mut() {
        output.logical_rect.x = x;
        x = x
            .checked_add(output.logical_rect.width)
            .ok_or(KmsTopologyError::LogicalCoordinateOverflow)?;
    }
    Ok(ConnectorResolution {
        outputs: resolved,
        rejections,
    })
}

fn ranked_connector_modes(connector: &ConnectorDescription) -> Vec<ConnectorMode> {
    let mut modes = connector.modes.clone();
    modes.sort_by_key(|mode| {
        (
            std::cmp::Reverse(scanout_mode_class(*mode)),
            std::cmp::Reverse(mode.preferred),
            std::cmp::Reverse(u64::from(mode.width) * u64::from(mode.height)),
            std::cmp::Reverse(mode.width),
            std::cmp::Reverse(mode.height),
            std::cmp::Reverse(mode.refresh_millihz),
        )
    });
    modes
}

pub(crate) fn scanout_mode_class(mode: ConnectorMode) -> u8 {
    if mode.flags & (DRM_MODE_FLAG_INTERLACE | DRM_MODE_FLAG_DBLSCAN) != 0
        || mode.hsync.2 == 0
        || mode.vsync.2 == 0
    {
        0
    } else if mode.vscan > 1 {
        1
    } else {
        2
    }
}

fn drm_mode_tuple_is_ambiguous(connector: &ConnectorDescription, candidate: ConnectorMode) -> bool {
    if scanout_mode_class(candidate) == 0 {
        return false;
    }
    connector.modes.iter().any(|other| {
        scanout_mode_class(*other) > 0
            && (other.width, other.height, other.refresh_millihz)
                == (candidate.width, candidate.height, candidate.refresh_millihz)
            && !same_drm_timing(*other, candidate)
    })
}

pub(crate) fn same_drm_timing(left: ConnectorMode, right: ConnectorMode) -> bool {
    left.width == right.width
        && left.height == right.height
        && left.refresh_millihz == right.refresh_millihz
        && left.clock_khz == right.clock_khz
        && left.hsync == right.hsync
        && left.vsync == right.vsync
        && left.hskew == right.hskew
        && left.vscan == right.vscan
        && left.flags == right.flags
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(connector_id: u32) -> OutputKey {
        OutputKey {
            device: 226,
            connector_name: format!("Virtual-{connector_id}"),
        }
    }

    fn mode(width: u32, height: u32, refresh_millihz: u32) -> ConnectorMode {
        ConnectorMode {
            width,
            height,
            refresh_millihz,
            preferred: true,
            clock_khz: 148_500,
            hsync: (2008, 2052, 2200),
            vsync: (1084, 1089, 1125),
            hskew: 0,
            vscan: 0,
            flags: 0,
        }
    }

    fn connector(connector_id: u32, mode: ConnectorMode) -> ConnectorDescription {
        ConnectorDescription {
            key: key(connector_id),
            connector_id,
            modes: vec![mode],
        }
    }

    fn snapshot(
        connector: ConnectorDescription,
        output_scale: OutputScale120,
    ) -> KmsTopologySnapshot {
        let connector_mode = connector.modes[0];
        KmsTopologySnapshot {
            connectors: vec![connector.clone()],
            selections: vec![PreselectedAtomicOutput {
                key: connector.key.clone(),
                connector_mode,
                selection: selector(&connector, connector_mode),
            }],
            output_scale,
        }
    }

    #[test]
    fn output_scale_120_keeps_exact_logical_arithmetic_and_boundary_conversion() {
        let scale = OutputScale120::new(300).expect("250 percent is positive");

        assert_eq!(scale.get(), 300);
        assert_eq!(scale.as_f64(), 2.5);
        assert_eq!(scale.to_string(), "2.5");
        assert_eq!(scale.logical_extent((3840, 2160)), Some((1536, 864)));
        assert_eq!(scale.logical_extent((1366, 768)), None);
        assert_eq!(OutputScale120::new(0), None);
        assert_eq!(OutputScale120::default(), OutputScale120::ONE);
    }

    #[test]
    fn fractional_topology_is_logical_while_drm_and_atomic_modes_stay_physical() {
        let scale = OutputScale120::new(300).expect("250 percent");
        let connector = connector(10, mode(3840, 2160, 60_000));
        let mut topology = KmsTopology::default();
        let commands = topology
            .reduce_lifecycle(KmsTopologyLifecycleEvent::Initial(snapshot(
                connector, scale,
            )))
            .expect("4K at 250 percent is integral");
        let [KmsRenderCommand::AddOutput { output, .. }] = commands.as_slice() else {
            panic!("one admitted connector emits one add: {commands:?}");
        };

        assert_eq!(
            (output.connector_mode.width, output.connector_mode.height),
            (3840, 2160),
            "DRM mode remains physical"
        );
        assert_eq!(
            (output.display.mode.width, output.display.mode.height),
            (3840, 2160),
            "Vulkan target mode remains physical"
        );
        assert_eq!(output.output_scale, scale);
        assert_eq!(
            output.logical_rect,
            LogicalRect {
                x: 0,
                y: 0,
                width: 1536,
                height: 864,
            }
        );
        assert_eq!(topology.seat_extent(), Some((1536, 864)));
        assert_eq!(
            topology.seat_regions(),
            [crate::backend::SeatRegion {
                x: 0.0,
                y: 0.0,
                width: 1536.0,
                height: 864.0,
            }]
        );
    }

    #[test]
    fn non_integral_logical_extent_is_rejected_before_atomic_selection() {
        let scale = OutputScale120::new(300).expect("250 percent");
        let connector = connector(10, mode(1366, 768, 60_000));
        let resolution = resolve_connectors(vec![connector], scale, &mut |_,
                                                                          _|
         -> Result<
            AtomicOutputSelection,
            String,
        > {
            panic!("a non-integral mode must not cross the atomic admission seam")
        })
        .expect("one unusable connector is a structured rejection");

        assert!(resolution.outputs.is_empty());
        let rejection = resolution
            .rejections
            .get(&key(10))
            .expect("connector rejection is retained");
        assert_eq!(rejection.reason_code, "non-integral-logical-extent");
        assert_eq!(rejection.rejected_mode_count, 1);
    }

    #[test]
    fn pause_resume_rebuild_preserves_fractional_logical_topology() {
        let scale = OutputScale120::new(300).expect("250 percent");
        let connector = connector(10, mode(3840, 2160, 60_000));
        let admitted = snapshot(connector, scale);
        let mut topology = KmsTopology::default();
        topology
            .reduce_lifecycle(KmsTopologyLifecycleEvent::Initial(admitted.clone()))
            .expect("initial topology");
        topology
            .reduce_lifecycle(KmsTopologyLifecycleEvent::Pause)
            .expect("pause topology");
        assert_eq!(
            topology.output_scale(),
            scale,
            "session pause drops DRM outputs but retains the admitted client scale"
        );
        let resumed = topology
            .reduce_lifecycle(KmsTopologyLifecycleEvent::Resume(admitted.clone()))
            .expect("resume with the admitted scale");
        assert!(matches!(
            resumed.as_slice(),
            [
                KmsRenderCommand::Resume { .. },
                KmsRenderCommand::AddOutput { output, .. }
            ] if output.output_scale == scale
                && (output.logical_rect.width, output.logical_rect.height) == (1536, 864)
                && (output.connector_mode.width, output.connector_mode.height) == (3840, 2160)
        ));

        topology
            .reduce_lifecycle(KmsTopologyLifecycleEvent::Pause)
            .expect("second pause");
        let physical_logical = KmsTopologySnapshot {
            output_scale: OutputScale120::ONE,
            ..admitted
        };
        assert_eq!(
            topology
                .reduce_lifecycle(KmsTopologyLifecycleEvent::Resume(physical_logical))
                .expect_err("resume cannot silently return to physical-logical geometry"),
            KmsTopologyError::OutputScaleChanged {
                previous: scale,
                resumed: OutputScale120::ONE,
            }
        );
    }

    #[test]
    fn seat_extent_is_absent_until_an_output_is_admitted() {
        assert_eq!(KmsTopology::default().seat_extent(), None);
    }

    #[test]
    fn seat_extent_follows_the_admitted_mode_rather_than_any_bootstrap_size() {
        let mut topology = KmsTopology::default();
        topology
            .reduce(
                KmsTopologyEvent::ConnectorScan(vec![connector(10, mode(1920, 1080, 60_000))]),
                &mut selector,
            )
            .expect("initial scan");

        assert_eq!(
            topology.seat_extent(),
            Some((1920, 1080)),
            "input is confined to the mode the connector actually runs"
        );
    }

    #[test]
    fn seat_extent_spans_every_admitted_output() {
        let mut topology = KmsTopology::default();
        topology
            .reduce(
                KmsTopologyEvent::ConnectorScan(vec![
                    connector(10, mode(1920, 1080, 60_000)),
                    connector(20, mode(1280, 1024, 60_000)),
                ]),
                &mut selector,
            )
            .expect("initial scan");

        let layouts = topology.output_layouts();
        assert_eq!(layouts.len(), 2, "both connectors are admitted");
        let far = layouts
            .values()
            .map(|rect| (rect.x + rect.width, rect.y + rect.height))
            .fold((0, 0), |far, edge| (far.0.max(edge.0), far.1.max(edge.1)));

        // A two-output seat is one continuous pointer space. Confining to
        // either output's own mode would make the other unreachable, so the
        // extent has to reach the far edge of the whole layout.
        assert_eq!(
            topology.seat_extent(),
            Some((far.0 as u32, far.1 as u32)),
            "the extent reaches the far edge of the layout, not one output's mode"
        );
        assert!(
            topology.seat_extent().expect("outputs are admitted").0 > 1920,
            "a side-by-side layout is wider than its widest output"
        );
    }

    #[test]
    fn kms_backend_seat_extent_answers_from_the_topology_not_the_bootstrap_extent() {
        // The topology knowing the right extent is worth nothing if the backend
        // accessor input actually calls still answers `output_size`. No KMS
        // event ever updates `output_size`, so on a real 1920x1080 seat that
        // would confine the pointer to the 960x640 the compositor booted at.
        //
        // This lives here rather than in `backend/mod.rs` because admitting an
        // output needs this module's connector and selector fixtures; the
        // private `topology` field is reachable because a private item is
        // visible to every descendant of the module that declares it.
        let mut data = crate::backend::KmsBackendData::new((960, 640));
        data.topology
            .reduce(
                KmsTopologyEvent::ConnectorScan(vec![connector(10, mode(1920, 1080, 60_000))]),
                &mut selector,
            )
            .expect("initial scan");
        let backend = crate::backend::BackendData::Kms(data);

        assert_eq!(
            backend.seat_extent(),
            (1920, 1080),
            "input is confined to the admitted layout"
        );
        assert_eq!(
            backend.output_size(),
            (960, 640),
            "the bootstrap extent is untouched, which is exactly why it cannot be the seat"
        );
    }

    #[test]
    fn kms_seat_regions_are_the_admitted_outputs_not_their_bounding_box() {
        let mut data = crate::backend::KmsBackendData::new((960, 640));
        data.topology
            .reduce(
                KmsTopologyEvent::ConnectorScan(vec![
                    connector(10, mode(1920, 1080, 60_000)),
                    connector(20, mode(1280, 1024, 60_000)),
                ]),
                &mut selector,
            )
            .expect("initial scan");
        let backend = crate::backend::BackendData::Kms(data);

        let regions = backend.seat_regions();
        assert_eq!(regions.len(), 2, "one region per admitted output");
        let covered: f64 = regions.iter().map(|r| r.width * r.height).sum();
        let (width, height) = backend.seat_extent();
        let boxed = f64::from(width) * f64::from(height);
        // The whole point of keeping the two apart: with unequal heights the
        // bounding box strictly exceeds what the outputs actually cover, and
        // the difference is where a bounding-box clamp would strand the cursor.
        assert!(
            covered < boxed,
            "unequal output heights leave dead space in the box: covered {covered}, box {boxed}"
        );
        assert_eq!(
            boxed - covered,
            1280.0 * (1080.0 - 1024.0),
            "the dead space is exactly the strip under the shorter output"
        );
    }

    #[test]
    fn kms_seat_regions_fall_back_to_the_bootstrap_extent_before_any_output() {
        let backend =
            crate::backend::BackendData::Kms(crate::backend::KmsBackendData::new((960, 640)));

        // Never empty: an empty region list would leave the confinement with
        // nothing to clamp against, and the pointer unconfined.
        assert_eq!(
            backend.seat_regions(),
            vec![crate::backend::SeatRegion {
                x: 0.0,
                y: 0.0,
                width: 960.0,
                height: 640.0
            }]
        );
    }

    #[test]
    fn protocol_owned_resume_consumes_only_the_fresh_atomic_selection() {
        let connector = connector(10, mode(1920, 1080, 60_000));
        let selected_mode = connector.modes[0];
        let initial_selection = AtomicOutputSelection {
            primary_plane_id: 3,
            ..selector(&connector, selected_mode).expect("atomic fixture")
        };
        let resumed_selection = AtomicOutputSelection {
            primary_plane_id: 7,
            ..selector(&connector, selected_mode).expect("atomic fixture")
        };
        let initial = KmsTopologySnapshot {
            connectors: vec![connector.clone()],
            selections: vec![PreselectedAtomicOutput {
                key: connector.key.clone(),
                connector_mode: selected_mode,
                selection: Ok(initial_selection),
            }],
            output_scale: OutputScale120::ONE,
        };
        let resumed = KmsTopologySnapshot {
            connectors: vec![connector.clone()],
            selections: vec![PreselectedAtomicOutput {
                key: connector.key.clone(),
                connector_mode: selected_mode,
                selection: Ok(resumed_selection),
            }],
            output_scale: OutputScale120::ONE,
        };
        let mut topology = KmsTopology::default();

        assert!(matches!(
            topology
                .reduce_lifecycle(KmsTopologyLifecycleEvent::Initial(initial))
                .expect("initial topology"),
            commands if matches!(commands.as_slice(), [KmsRenderCommand::AddOutput { generation: 1, output }]
                if output.display == initial_selection)
        ));
        assert_eq!(
            topology
                .reduce_lifecycle(KmsTopologyLifecycleEvent::Pause)
                .expect("pause topology"),
            [KmsRenderCommand::Suspend { generation: 2 }]
        );
        let resumed_commands = topology
            .reduce_lifecycle(KmsTopologyLifecycleEvent::Resume(resumed))
            .expect("resume topology");
        assert!(matches!(
            resumed_commands.as_slice(),
            [
                KmsRenderCommand::Resume { generation: 3 },
                KmsRenderCommand::AddOutput { generation: 4, output }
            ]
                if output.display == resumed_selection && output.display != initial_selection
        ));
    }

    #[test]
    fn current_selector_constructs_the_atomic_selection() {
        let connector = connector(10, mode(1920, 1080, 60_000));
        let mut topology = KmsTopology::default();
        let commands = topology
            .reduce(
                KmsTopologyEvent::ConnectorScan(vec![connector]),
                &mut selector,
            )
            .expect("the current atomic selector admits the output");
        let [KmsRenderCommand::AddOutput { output, .. }] = commands.as_slice() else {
            panic!("one current output must be added: {commands:?}");
        };

        assert_eq!(output.display.connector_id, output.connector_id);
    }

    #[test]
    fn atomic_output_selection_contract_retains_route_and_scanout_format() {
        let mode = mode(2560, 1440, 144_000);
        let selection = AtomicOutputSelection {
            connector_id: 17,
            crtc_id: 23,
            primary_plane_id: 31,
            mode,
            format: 0x3432_5258,
            modifier: 0x0100_0000_0000_0002,
        };
        assert_eq!(selection.mode.refresh_millihz, 144_000);
        assert_eq!(selection.primary_plane_id, 31);
    }

    fn selector(
        connector: &ConnectorDescription,
        selected: ConnectorMode,
    ) -> Result<AtomicOutputSelection, String> {
        Ok(AtomicOutputSelection {
            connector_id: connector.connector_id,
            crtc_id: connector.connector_id.saturating_add(100),
            primary_plane_id: connector.connector_id.saturating_add(200),
            mode: selected,
            format: 0x3432_5258,
            modifier: 0,
        })
    }

    #[test]
    fn progressive_mode_is_ranked_ahead_of_preferred_interlace() {
        let mut interlaced = mode(1920, 1080, 60_000);
        interlaced.flags = DRM_MODE_FLAG_INTERLACE;
        let progressive = ConnectorMode {
            preferred: false,
            ..mode(1920, 1080, 60_000)
        };
        let connector = ConnectorDescription {
            key: key(7),
            connector_id: 7,
            modes: vec![interlaced, progressive],
        };
        let mut calls = Vec::new();
        let mut choose =
            |connector: &ConnectorDescription, selected: ConnectorMode| -> Result<_, String> {
                calls.push(selected);
                selector(connector, selected)
            };

        let resolved = resolve_connectors(vec![connector], OutputScale120::ONE, &mut choose)
            .expect("valid scan");

        assert_eq!(calls, [progressive]);
        assert_eq!(
            resolved
                .outputs
                .get(&key(7))
                .map(|output| output.connector_mode),
            Some(progressive)
        );
    }

    #[test]
    fn progressive_mode_is_ranked_ahead_of_preferred_doublescan() {
        let mut doublescan = mode(1920, 1080, 30_000);
        doublescan.flags = DRM_MODE_FLAG_DBLSCAN;
        let progressive = ConnectorMode {
            preferred: false,
            ..mode(1920, 1080, 60_000)
        };
        let connector = ConnectorDescription {
            key: key(7),
            connector_id: 7,
            modes: vec![doublescan, progressive],
        };
        let mut calls = Vec::new();
        let mut choose =
            |connector: &ConnectorDescription, selected: ConnectorMode| -> Result<_, String> {
                calls.push(selected);
                selector(connector, selected)
            };

        resolve_connectors(vec![connector], OutputScale120::ONE, &mut choose).expect("valid scan");

        assert_eq!(calls, [progressive]);
    }

    #[test]
    fn ordinary_progressive_mode_is_ranked_ahead_of_compatible_vscan() {
        let mut vscan = mode(1920, 1080, 30_000);
        vscan.vscan = 2;
        let progressive = ConnectorMode {
            preferred: false,
            ..mode(1920, 1080, 60_000)
        };
        let connector = ConnectorDescription {
            key: key(7),
            connector_id: 7,
            modes: vec![vscan, progressive],
        };
        let mut calls = Vec::new();
        let mut choose =
            |connector: &ConnectorDescription, selected: ConnectorMode| -> Result<_, String> {
                calls.push(selected);
                selector(connector, selected)
            };

        resolve_connectors(vec![connector], OutputScale120::ONE, &mut choose).expect("valid scan");

        assert_eq!(calls, [progressive]);
    }

    #[test]
    fn real_totals_are_ranked_ahead_of_a_preferred_fallback_timing() {
        let mut fallback = mode(1920, 1080, 60_000);
        fallback.hsync.2 = 0;
        let progressive = ConnectorMode {
            preferred: false,
            ..mode(1920, 1080, 60_000)
        };
        let connector = ConnectorDescription {
            key: key(7),
            connector_id: 7,
            modes: vec![fallback, progressive],
        };
        let mut calls = Vec::new();
        let mut choose =
            |connector: &ConnectorDescription, selected: ConnectorMode| -> Result<_, String> {
                calls.push(selected);
                selector(connector, selected)
            };

        resolve_connectors(vec![connector], OutputScale120::ONE, &mut choose).expect("valid scan");

        assert_eq!(calls, [progressive]);
    }

    #[test]
    fn real_vertical_total_is_ranked_ahead_of_a_preferred_fallback_timing() {
        let mut fallback = mode(1920, 1080, 60_000);
        fallback.vsync.2 = 0;
        let progressive = ConnectorMode {
            preferred: false,
            ..mode(1920, 1080, 60_000)
        };
        let connector = ConnectorDescription {
            key: key(7),
            connector_id: 7,
            modes: vec![fallback, progressive],
        };
        let mut calls = Vec::new();
        let mut choose =
            |connector: &ConnectorDescription, selected: ConnectorMode| -> Result<_, String> {
                calls.push(selected);
                selector(connector, selected)
            };

        resolve_connectors(vec![connector], OutputScale120::ONE, &mut choose).expect("valid scan");

        assert_eq!(calls, [progressive]);
    }

    #[test]
    fn mode_specific_selection_failure_tries_the_next_ranked_candidate() {
        let preferred = mode(2560, 1440, 144_000);
        let fallback = ConnectorMode {
            preferred: false,
            ..mode(1920, 1080, 60_000)
        };
        let connector = ConnectorDescription {
            key: key(7),
            connector_id: 7,
            modes: vec![preferred, fallback],
        };
        let mut calls = Vec::new();
        let mut choose =
            |connector: &ConnectorDescription, selected: ConnectorMode| -> Result<_, String> {
                calls.push(selected);
                if selected == preferred {
                    Err("preferred mode is not advertised by Vulkan".into())
                } else {
                    selector(connector, selected)
                }
            };

        let resolved = resolve_connectors(vec![connector], OutputScale120::ONE, &mut choose)
            .expect("valid scan");

        assert_eq!(calls, [preferred, fallback]);
        assert_eq!(
            resolved
                .outputs
                .get(&key(7))
                .map(|output| output.connector_mode),
            Some(fallback)
        );
    }

    #[test]
    fn full_timing_mismatched_selection_tries_the_next_ranked_candidate() {
        let preferred = mode(2560, 1440, 144_000);
        let fallback = ConnectorMode {
            preferred: false,
            ..mode(1920, 1080, 60_000)
        };
        let connector = ConnectorDescription {
            key: key(7),
            connector_id: 7,
            modes: vec![preferred, fallback],
        };
        let mut choose =
            |connector: &ConnectorDescription, selected: ConnectorMode| -> Result<_, String> {
                if selected == preferred {
                    Ok(AtomicOutputSelection {
                        mode: mode(1920, 1080, selected.refresh_millihz),
                        ..selector(connector, selected)?
                    })
                } else {
                    selector(connector, selected)
                }
            };

        let resolved = resolve_connectors(vec![connector], OutputScale120::ONE, &mut choose)
            .expect("valid scan");

        assert_eq!(
            resolved
                .outputs
                .get(&key(7))
                .map(|output| output.connector_mode),
            Some(fallback)
        );
    }

    #[test]
    fn clock_only_mismatched_selection_tries_the_next_ranked_candidate() {
        let preferred = mode(2560, 1440, 144_000);
        let fallback = ConnectorMode {
            preferred: false,
            ..mode(1920, 1080, 60_000)
        };
        let connector = ConnectorDescription {
            key: key(7),
            connector_id: 7,
            modes: vec![preferred, fallback],
        };
        let mut choose =
            |connector: &ConnectorDescription, selected: ConnectorMode| -> Result<_, String> {
                let mut display = selector(connector, selected)?;
                if selected == preferred {
                    display.mode.clock_khz += 1;
                }
                Ok(display)
            };

        let resolved = resolve_connectors(vec![connector], OutputScale120::ONE, &mut choose)
            .expect("valid fallback scan");

        assert_eq!(
            resolved
                .outputs
                .get(&key(7))
                .map(|output| output.connector_mode),
            Some(fallback)
        );
    }

    #[test]
    fn connector_id_mismatched_selection_tries_the_next_ranked_candidate() {
        let preferred = mode(2560, 1440, 144_000);
        let fallback = ConnectorMode {
            preferred: false,
            ..mode(1920, 1080, 60_000)
        };
        let connector = ConnectorDescription {
            key: key(7),
            connector_id: 7,
            modes: vec![preferred, fallback],
        };
        let mut choose =
            |connector: &ConnectorDescription, selected: ConnectorMode| -> Result<_, String> {
                let mut display = selector(connector, selected)?;
                if selected == preferred {
                    display.connector_id += 1;
                }
                Ok(display)
            };

        let resolved = resolve_connectors(vec![connector], OutputScale120::ONE, &mut choose)
            .expect("valid fallback scan");

        assert_eq!(
            resolved
                .outputs
                .get(&key(7))
                .map(|output| output.connector_mode),
            Some(fallback)
        );
    }

    #[test]
    fn duplicate_connector_identity_is_the_sole_scan_falsifier() {
        let duplicate = connector(7, mode(1920, 1080, 60_000));
        let mut choose = selector;

        assert_eq!(
            resolve_connectors(
                vec![duplicate.clone(), duplicate],
                OutputScale120::ONE,
                &mut choose,
            ),
            Err(KmsTopologyError::DuplicateConnector(key(7)))
        );
    }

    #[test]
    fn distinct_drm_timing_with_the_same_tuple_is_the_sole_ambiguity_falsifier() {
        let first = mode(1920, 1080, 60_000);
        let second = ConnectorMode {
            clock_khz: 140_400,
            hsync: (1960, 2000, 2080),
            ..first
        };
        let connector = ConnectorDescription {
            key: key(7),
            connector_id: 7,
            modes: vec![first, second],
        };
        let mut choose = selector;

        let resolved = resolve_connectors(vec![connector], OutputScale120::ONE, &mut choose)
            .expect("valid scan");

        assert!(resolved.outputs.is_empty());
        let rejection = resolved
            .rejections
            .get(&key(7))
            .expect("ambiguous connector is retained as structured data");
        assert_eq!(rejection.candidate_mode_count, 2);
        assert_eq!(rejection.rejected_mode_count, 2);
        assert_eq!(rejection.reason_code, "ambiguous-drm-mode-tuple");
    }

    #[test]
    fn ambiguous_drm_tuple_falls_back_to_a_unique_candidate() {
        let first = mode(1920, 1080, 60_000);
        let second = ConnectorMode {
            clock_khz: 140_400,
            hsync: (1960, 2000, 2080),
            ..first
        };
        let fallback = ConnectorMode {
            preferred: false,
            ..mode(1280, 720, 60_000)
        };
        let connector = ConnectorDescription {
            key: key(7),
            connector_id: 7,
            modes: vec![first, second, fallback],
        };
        let mut choose = selector;

        let resolved = resolve_connectors(vec![connector], OutputScale120::ONE, &mut choose)
            .expect("valid scan");

        assert_eq!(
            resolved
                .outputs
                .get(&key(7))
                .map(|output| output.connector_mode),
            Some(fallback)
        );
    }

    #[test]
    fn connector_without_a_candidate_does_not_blank_a_working_peer() {
        let rejected = connector(10, mode(1920, 1080, 60_000));
        let working = connector(20, mode(1280, 720, 60_000));
        let rejected_key = rejected.key.clone();
        let mut choose =
            |connector: &ConnectorDescription, selected: ConnectorMode| -> Result<_, String> {
                if connector.key == rejected_key {
                    Err("no compatible Vulkan mode".into())
                } else {
                    selector(connector, selected)
                }
            };

        let resolved =
            resolve_connectors(vec![rejected, working], OutputScale120::ONE, &mut choose)
                .expect("valid scan");

        assert_eq!(
            resolved.outputs.keys().cloned().collect::<Vec<_>>(),
            [key(20)]
        );
        assert_eq!(
            resolved.rejections.keys().cloned().collect::<Vec<_>>(),
            [key(10)]
        );
        let rejection = resolved
            .rejections
            .get(&key(10))
            .expect("rejected peer is first-class data");
        assert_eq!(rejection.candidate_mode_count, 1);
        assert_eq!(rejection.rejected_mode_count, 1);
        assert_eq!(
            rejection.highest_ranked_candidate,
            Some(ConnectorModeTuple {
                width: 1920,
                height: 1080,
                refresh_millihz: 60_000,
            })
        );
        assert_eq!(rejection.reason_code, "display-selection-rejected");
        assert_eq!(rejection.reason, "no compatible Vulkan mode");
    }

    #[test]
    fn structured_rejection_retains_the_highest_ranked_candidate_reason() {
        let preferred = mode(2560, 1440, 144_000);
        let fallback = ConnectorMode {
            preferred: false,
            ..mode(1920, 1080, 60_000)
        };
        let connector = ConnectorDescription {
            key: key(7),
            connector_id: 7,
            modes: vec![fallback, preferred],
        };
        let mut choose = |_: &ConnectorDescription,
                          selected: ConnectorMode|
         -> Result<AtomicOutputSelection, String> {
            Err(if selected == preferred {
                "highest-ranked rejection"
            } else {
                "lower-ranked rejection"
            }
            .into())
        };

        let resolution = resolve_connectors(vec![connector], OutputScale120::ONE, &mut choose)
            .expect("valid scan");
        let rejection = resolution
            .rejections
            .get(&key(7))
            .expect("connector rejection");

        assert_eq!(rejection.candidate_mode_count, 2);
        assert_eq!(rejection.rejected_mode_count, 2);
        assert_eq!(
            rejection.highest_ranked_candidate,
            Some(ConnectorModeTuple {
                width: 2560,
                height: 1440,
                refresh_millihz: 144_000,
            })
        );
        assert_eq!(rejection.reason_code, "display-selection-rejected");
        assert_eq!(rejection.reason, "highest-ranked rejection");
    }

    #[test]
    fn connector_without_modes_has_a_stable_rejection_code() {
        let connector = ConnectorDescription {
            key: key(7),
            connector_id: 7,
            modes: Vec::new(),
        };
        let mut choose = selector;

        let resolution = resolve_connectors(vec![connector], OutputScale120::ONE, &mut choose)
            .expect("valid scan");
        let rejection = resolution
            .rejections
            .get(&key(7))
            .expect("empty connector rejection");

        assert_eq!(rejection.candidate_mode_count, 0);
        assert_eq!(rejection.rejected_mode_count, 0);
        assert_eq!(rejection.highest_ranked_candidate, None);
        assert_eq!(rejection.reason_code, "no-candidate-modes");
        assert_eq!(rejection.reason, "connector advertised no modes");
    }

    #[test]
    fn newly_unusable_connector_is_removed_while_its_peer_remains() {
        let first = connector(10, mode(1920, 1080, 60_000));
        let second = connector(20, mode(1280, 720, 60_000));
        let mut topology = KmsTopology::default();
        topology
            .reduce(
                KmsTopologyEvent::ConnectorScan(vec![first.clone(), second.clone()]),
                &mut selector,
            )
            .expect("initial scan");

        let first_key = first.key.clone();
        let mut choose =
            |connector: &ConnectorDescription, selected: ConnectorMode| -> Result<_, String> {
                if connector.key == first_key {
                    Err("no compatible Vulkan mode".into())
                } else {
                    selector(connector, selected)
                }
            };
        let commands = topology
            .reduce(
                KmsTopologyEvent::ConnectorScan(vec![first, second]),
                &mut choose,
            )
            .expect("one connector can be refused independently");

        assert_eq!(
            topology.output(key(10)).map(|output| output.phase),
            Some(OutputPhase::Removing)
        );
        assert!(topology.output(key(20)).is_some());
        assert_eq!(
            topology.logical_rect(key(20)),
            Some(LogicalRect {
                x: 0,
                y: 0,
                width: 1280,
                height: 720,
            })
        );
        assert!(commands.iter().any(
            |command| matches!(command, KmsRenderCommand::RemoveOutput { key, .. } if *key == self::key(10))
        ));
        assert!(commands.iter().all(
            |command| !matches!(command, KmsRenderCommand::RemoveOutput { key, .. } if *key == self::key(20))
        ));
    }

    fn command_generation(command: &KmsRenderCommand) -> u64 {
        match command {
            KmsRenderCommand::Suspend { generation }
            | KmsRenderCommand::Resume { generation }
            | KmsRenderCommand::AddOutput { generation, .. }
            | KmsRenderCommand::ChangeOutput { generation, .. }
            | KmsRenderCommand::RemoveOutput { generation, .. } => *generation,
        }
    }

    #[test]
    fn connector_add_change_remove_emits_incarnation_commands() {
        let mut topology = KmsTopology::default();
        let first = connector(7, mode(1920, 1080, 60_000));
        let commands = topology
            .reduce(
                KmsTopologyEvent::ConnectorScan(vec![first.clone()]),
                &mut selector,
            )
            .expect("initial scan");
        let [
            KmsRenderCommand::AddOutput {
                generation: add_generation,
                output,
            },
        ] = commands.as_slice()
        else {
            panic!("initial connector must emit AddOutput");
        };
        assert_eq!(output.key, first.key);

        topology
            .reduce(
                KmsTopologyEvent::RenderReply(KmsRenderReply::OutputReady {
                    generation: *add_generation,
                    key: first.key.clone(),
                }),
                &mut selector,
            )
            .expect("ready reply");
        assert_eq!(
            topology
                .output(first.key.clone())
                .map(|output| output.phase),
            Some(OutputPhase::Ready)
        );

        let changed = connector(7, mode(2560, 1440, 144_000));
        let commands = topology
            .reduce(
                KmsTopologyEvent::UdevChange(vec![changed.clone()]),
                &mut selector,
            )
            .expect("changed scan");
        let [
            KmsRenderCommand::ChangeOutput {
                generation: change_generation,
                output,
            },
        ] = commands.as_slice()
        else {
            panic!("changed connector must emit ChangeOutput");
        };
        assert_ne!(change_generation, add_generation);
        assert_eq!(output.display.mode.refresh_millihz, 144_000);

        let commands = topology
            .reduce(KmsTopologyEvent::UdevChange(Vec::new()), &mut selector)
            .expect("removed scan");
        let [
            KmsRenderCommand::RemoveOutput {
                generation: remove_generation,
                key,
            },
        ] = commands.as_slice()
        else {
            panic!("missing connector must emit RemoveOutput");
        };
        assert_eq!(key, &first.key);
        assert_ne!(remove_generation, change_generation);
        assert_eq!(
            topology
                .output(first.key.clone())
                .map(|output| (output.generation, output.phase)),
            Some((*remove_generation, OutputPhase::Removing))
        );
        assert!(topology.logical_rect(first.key.clone()).is_none());

        topology
            .reduce(
                KmsTopologyEvent::RenderReply(KmsRenderReply::OutputRemoved {
                    generation: *remove_generation,
                    key: first.key.clone(),
                }),
                &mut selector,
            )
            .expect("removed acknowledgement");
        assert!(topology.output(first.key.clone()).is_none());
    }

    #[test]
    fn stale_output_ready_cannot_resurrect_removed_connector() {
        let mut topology = KmsTopology::default();
        let connector = connector(9, mode(1920, 1080, 60_000));
        let add = topology
            .reduce(
                KmsTopologyEvent::ConnectorScan(vec![connector.clone()]),
                &mut selector,
            )
            .expect("initial scan");
        let add_generation = command_generation(&add[0]);
        let remove = topology
            .reduce(KmsTopologyEvent::UdevChange(Vec::new()), &mut selector)
            .expect("connector removal");
        let remove_generation = command_generation(&remove[0]);

        let commands = topology
            .reduce(
                KmsTopologyEvent::RenderReply(KmsRenderReply::OutputReady {
                    generation: add_generation,
                    key: connector.key.clone(),
                }),
                &mut selector,
            )
            .expect("stale ready reply");
        assert!(commands.is_empty());
        assert_eq!(
            topology
                .output(connector.key.clone())
                .map(|output| (output.generation, output.phase)),
            Some((remove_generation, OutputPhase::Removing))
        );
        assert!(topology.logical_rect(connector.key.clone()).is_none());
        assert_eq!(topology.ignored_render_replies(), 1);
        assert_eq!(
            topology
                .last_ignored_render_reply()
                .map(|ignored| &ignored.reason),
            Some(&IgnoredRenderReplyReason::SupersededGeneration {
                expected: remove_generation,
            })
        );
    }

    #[test]
    fn stale_output_ready_cannot_ready_replugged_connector() {
        let mut topology = KmsTopology::default();
        let connector = connector(9, mode(1920, 1080, 60_000));
        let first_add = topology
            .reduce(
                KmsTopologyEvent::ConnectorScan(vec![connector.clone()]),
                &mut selector,
            )
            .expect("initial scan");
        let stale_generation = command_generation(&first_add[0]);
        topology
            .reduce(KmsTopologyEvent::UdevChange(Vec::new()), &mut selector)
            .expect("connector removal");
        let second_add = topology
            .reduce(
                KmsTopologyEvent::UdevChange(vec![connector.clone()]),
                &mut selector,
            )
            .expect("connector replug");
        let current_generation = command_generation(&second_add[0]);
        assert_ne!(current_generation, stale_generation);
        assert_eq!(
            topology
                .output(connector.key.clone())
                .map(|output| (output.generation, output.phase)),
            Some((current_generation, OutputPhase::Adding))
        );
        topology
            .reduce(
                KmsTopologyEvent::RenderReply(KmsRenderReply::OutputReady {
                    generation: stale_generation,
                    key: connector.key.clone(),
                }),
                &mut selector,
            )
            .expect("stale ready reply");

        assert_eq!(
            topology
                .output(connector.key.clone())
                .map(|output| (output.generation, output.phase)),
            Some((current_generation, OutputPhase::Adding))
        );
        assert_eq!(topology.ignored_render_replies(), 1);
    }

    #[test]
    fn stale_output_removed_cannot_remove_a_replugged_generation() {
        let mut topology = KmsTopology::default();
        let connector = connector(12, mode(1920, 1080, 60_000));
        topology
            .reduce(
                KmsTopologyEvent::ConnectorScan(vec![connector.clone()]),
                &mut selector,
            )
            .expect("initial scan");
        let remove = topology
            .reduce(KmsTopologyEvent::UdevChange(Vec::new()), &mut selector)
            .expect("remove scan");
        let remove_generation = command_generation(&remove[0]);
        let replug = topology
            .reduce(
                KmsTopologyEvent::UdevChange(vec![connector.clone()]),
                &mut selector,
            )
            .expect("replug scan");
        let replug_generation = command_generation(&replug[0]);

        topology
            .reduce(
                KmsTopologyEvent::RenderReply(KmsRenderReply::OutputRemoved {
                    generation: remove_generation,
                    key: connector.key.clone(),
                }),
                &mut selector,
            )
            .expect("stale removed reply");

        assert_eq!(
            topology
                .output(connector.key.clone())
                .map(|output| (output.generation, output.phase)),
            Some((replug_generation, OutputPhase::Adding))
        );
        assert_eq!(
            topology
                .last_ignored_render_reply()
                .map(|ignored| &ignored.reason),
            Some(&IgnoredRenderReplyReason::SupersededGeneration {
                expected: replug_generation,
            })
        );
    }

    #[test]
    fn failed_removal_is_terminal_and_does_not_depend_on_another_udev_event() {
        let mut topology = KmsTopology::default();
        let connector = connector(13, mode(1280, 720, 60_000));
        topology
            .reduce(
                KmsTopologyEvent::ConnectorScan(vec![connector.clone()]),
                &mut selector,
            )
            .expect("initial scan");
        let removal = topology
            .reduce(KmsTopologyEvent::UdevChange(Vec::new()), &mut selector)
            .expect("remove scan");
        let failed_generation = command_generation(&removal[0]);

        topology
            .reduce(
                KmsTopologyEvent::RenderReply(KmsRenderReply::OutputFailed {
                    generation: failed_generation,
                    key: connector.key.clone(),
                    reason: "fake remove refused".into(),
                }),
                &mut selector,
            )
            .expect("failed removal reply");
        assert_eq!(
            topology
                .output(connector.key.clone())
                .map(|output| output.phase),
            Some(OutputPhase::RemovalFailed)
        );
        assert!(topology.logical_rect(connector.key.clone()).is_none());
        assert_eq!(
            topology
                .failure(connector.key.clone())
                .map(|failure| failure.reason.as_str()),
            Some("fake remove refused")
        );

        let retry = topology
            .reduce(KmsTopologyEvent::UdevChange(Vec::new()), &mut selector)
            .expect("repeated absent scan");
        assert!(retry.is_empty(), "terminal removal failure must not retry");
    }

    #[test]
    fn terminal_worker_failure_is_typed_and_cannot_mutate_topology() {
        let mut topology = KmsTopology::default();
        let generation = topology.generation();
        let error = topology
            .reduce(
                KmsTopologyEvent::RenderReply(KmsRenderReply::WorkerFailed {
                    operation: KmsRenderOperation::Resume,
                    generation: 77,
                    key: None,
                    code: "fake-resume-refused",
                    reason: "injected resume failure".into(),
                }),
                &mut selector,
            )
            .expect_err("terminal worker failure must fail closed");

        assert!(matches!(
            error,
            KmsTopologyError::RenderWorkerFailed {
                operation: KmsRenderOperation::Resume,
                generation: 77,
                code: "fake-resume-refused",
                ..
            }
        ));
        assert_eq!(topology.generation(), generation);
    }

    #[test]
    fn suspend_overtakes_an_inflight_add_output() {
        let mut topology = KmsTopology::default();
        let connector = connector(11, mode(1280, 720, 60_000));
        let add = topology
            .reduce(
                KmsTopologyEvent::ConnectorScan(vec![connector.clone()]),
                &mut selector,
            )
            .expect("initial scan");
        let add_generation = command_generation(&add[0]);
        let suspend = topology
            .reduce(KmsTopologyEvent::SessionPause, &mut selector)
            .expect("pause");
        let [
            KmsRenderCommand::Suspend {
                generation: suspend_generation,
            },
        ] = suspend.as_slice()
        else {
            panic!("pause must emit Suspend");
        };
        assert!(!topology.is_active());
        assert!(topology.output(connector.key.clone()).is_none());
        assert_ne!(*suspend_generation, add_generation);

        topology
            .reduce(
                KmsTopologyEvent::RenderReply(KmsRenderReply::OutputReady {
                    generation: add_generation,
                    key: connector.key.clone(),
                }),
                &mut selector,
            )
            .expect("overtaken reply");
        assert!(topology.output(connector.key.clone()).is_none());

        topology
            .reduce(
                KmsTopologyEvent::RenderReply(KmsRenderReply::Suspended {
                    generation: *suspend_generation,
                }),
                &mut selector,
            )
            .expect("suspended reply");
        assert!(topology.suspend_confirmed());
    }

    #[test]
    fn resume_rebuilds_every_output_from_the_complete_scan() {
        let mut topology = KmsTopology::default();
        topology
            .reduce(
                KmsTopologyEvent::ConnectorScan(vec![
                    connector(1, mode(1920, 1080, 60_000)),
                    connector(2, mode(1920, 1080, 60_000)),
                ]),
                &mut selector,
            )
            .expect("initial scan");
        topology
            .reduce(KmsTopologyEvent::SessionPause, &mut selector)
            .expect("pause");

        let commands = topology
            .reduce(
                KmsTopologyEvent::SessionResume(vec![
                    connector(2, mode(2560, 1440, 144_000)),
                    connector(3, mode(1024, 768, 60_000)),
                ]),
                &mut selector,
            )
            .expect("resume scan");
        assert!(matches!(
            commands.first(),
            Some(KmsRenderCommand::Resume { .. })
        ));
        assert_eq!(
            commands
                .iter()
                .filter(|command| matches!(command, KmsRenderCommand::AddOutput { .. }))
                .count(),
            2
        );
        assert!(topology.output(key(1)).is_none());
        assert_eq!(
            topology
                .output(key(2))
                .map(|output| output.selected.display.mode.refresh_millihz),
            Some(144_000)
        );
        assert!(topology.output(key(3)).is_some());
    }

    #[test]
    fn exact_atomic_mode_and_plane_selection_are_preserved() {
        let mut topology = KmsTopology::default();
        let connector = ConnectorDescription {
            key: key(5),
            connector_id: 5,
            modes: vec![
                ConnectorMode {
                    preferred: false,
                    ..mode(3840, 2160, 30_000)
                },
                mode(1920, 1080, 60_000),
                mode(1920, 1080, 59_940),
            ],
        };
        let mut exact_selector =
            |_: &ConnectorDescription, selected: ConnectorMode| -> Result<_, String> {
                assert_eq!(selected.refresh_millihz, 60_000);
                Ok(AtomicOutputSelection {
                    connector_id: 5,
                    crtc_id: 105,
                    primary_plane_id: 3,
                    mode: selected,
                    format: u32::from_le_bytes(*b"XR24"),
                    modifier: 0,
                })
            };
        let commands = topology
            .reduce(
                KmsTopologyEvent::ConnectorScan(vec![connector]),
                &mut exact_selector,
            )
            .expect("exact atomic selection");
        let [KmsRenderCommand::AddOutput { output, .. }] = commands.as_slice() else {
            panic!("selected connector must be added");
        };
        assert_eq!(output.display.mode.refresh_millihz, 60_000);
        assert_eq!(output.display.primary_plane_id, 3);
        assert_eq!(output.connector_id, 5);
    }

    #[test]
    fn multi_output_layout_is_left_to_right_in_connector_order() {
        let mut topology = KmsTopology::default();
        topology
            .reduce(
                KmsTopologyEvent::ConnectorScan(vec![
                    connector(30, mode(1024, 768, 60_000)),
                    connector(10, mode(1920, 1080, 60_000)),
                    connector(20, mode(1280, 720, 60_000)),
                ]),
                &mut selector,
            )
            .expect("multi-output scan");

        assert_eq!(
            topology.logical_rect(key(10)),
            Some(LogicalRect {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            })
        );
        assert_eq!(
            topology.logical_rect(key(20)).map(|rect| rect.x),
            Some(1920)
        );
        assert_eq!(
            topology.logical_rect(key(30)).map(|rect| rect.x),
            Some(3200)
        );
    }

    #[test]
    fn output_failed_drops_the_incarnation_and_reflows_following_outputs() {
        let mut topology = KmsTopology::default();
        let commands = topology
            .reduce(
                KmsTopologyEvent::ConnectorScan(vec![
                    connector(1, mode(1920, 1080, 60_000)),
                    connector(2, mode(1280, 720, 60_000)),
                ]),
                &mut selector,
            )
            .expect("initial scan");
        let failed_generation = commands
            .iter()
            .find_map(|command| match command {
                KmsRenderCommand::AddOutput { generation, output } if output.key == key(1) => {
                    Some(*generation)
                }
                _ => None,
            })
            .expect("first output generation");

        let commands = topology
            .reduce(
                KmsTopologyEvent::RenderReply(KmsRenderReply::OutputFailed {
                    generation: failed_generation,
                    key: key(1),
                    reason: "surface creation failed".into(),
                }),
                &mut selector,
            )
            .expect("failed reply");
        assert!(topology.output(key(1)).is_none());
        assert_eq!(
            topology
                .failure(key(1))
                .map(|failure| failure.reason.as_str()),
            Some("surface creation failed")
        );
        assert_eq!(topology.logical_rect(key(2)).map(|rect| rect.x), Some(0));
        assert!(matches!(
            commands.as_slice(),
            [KmsRenderCommand::ChangeOutput { output, .. }] if output.key == key(2)
        ));
    }

    #[test]
    fn stale_output_failed_cannot_drop_or_reflow_replugged_connector() {
        let mut topology = KmsTopology::default();
        let target = connector(1, mode(1920, 1080, 60_000));
        let trailing = connector(2, mode(1280, 720, 60_000));
        let first_add = topology
            .reduce(
                KmsTopologyEvent::ConnectorScan(vec![target.clone()]),
                &mut selector,
            )
            .expect("initial scan");
        let stale_generation = command_generation(&first_add[0]);
        topology
            .reduce(KmsTopologyEvent::UdevChange(Vec::new()), &mut selector)
            .expect("connector removal");
        let second_add = topology
            .reduce(
                KmsTopologyEvent::UdevChange(vec![target.clone(), trailing.clone()]),
                &mut selector,
            )
            .expect("connector replug");
        let current_generation = second_add
            .iter()
            .find_map(|command| match command {
                KmsRenderCommand::AddOutput { generation, output } if output.key == target.key => {
                    Some(*generation)
                }
                _ => None,
            })
            .expect("replugged output generation");
        assert_ne!(current_generation, stale_generation);
        assert_eq!(
            topology
                .output(target.key.clone())
                .map(|output| (output.generation, output.phase)),
            Some((current_generation, OutputPhase::Adding))
        );
        let trailing_rect = topology
            .logical_rect(trailing.key.clone())
            .expect("trailing output layout");

        let commands = topology
            .reduce(
                KmsTopologyEvent::RenderReply(KmsRenderReply::OutputFailed {
                    generation: stale_generation,
                    key: target.key.clone(),
                    reason: "late surface failure".into(),
                }),
                &mut selector,
            )
            .expect("stale failed reply");

        assert!(commands.is_empty());
        assert_eq!(
            topology
                .output(target.key.clone())
                .map(|output| (output.generation, output.phase)),
            Some((current_generation, OutputPhase::Adding))
        );
        assert!(topology.failure(target.key.clone()).is_none());
        assert_eq!(
            topology.logical_rect(trailing.key.clone()),
            Some(trailing_rect)
        );
    }

    #[test]
    fn frame_submitted_updates_only_the_matching_ready_incarnation() {
        let mut topology = KmsTopology::default();
        let connector = connector(4, mode(800, 600, 60_000));
        let commands = topology
            .reduce(
                KmsTopologyEvent::ConnectorScan(vec![connector.clone()]),
                &mut selector,
            )
            .expect("initial scan");
        let generation = command_generation(&commands[0]);
        topology
            .reduce(
                KmsTopologyEvent::RenderReply(KmsRenderReply::FrameSubmitted {
                    generation,
                    key: connector.key.clone(),
                }),
                &mut selector,
            )
            .expect("premature frame reply");
        assert_eq!(
            topology
                .output(connector.key.clone())
                .map(|output| output.frames_submitted),
            Some(0)
        );

        topology
            .reduce(
                KmsTopologyEvent::RenderReply(KmsRenderReply::OutputReady {
                    generation,
                    key: connector.key.clone(),
                }),
                &mut selector,
            )
            .expect("ready reply");
        topology
            .reduce(
                KmsTopologyEvent::RenderReply(KmsRenderReply::FrameSubmitted {
                    generation,
                    key: connector.key.clone(),
                }),
                &mut selector,
            )
            .expect("submitted frame");
        assert_eq!(
            topology
                .output(connector.key.clone())
                .map(|output| output.frames_submitted),
            Some(1)
        );
    }

    #[test]
    fn stale_frame_submitted_cannot_increment_replugged_connector() {
        let mut topology = KmsTopology::default();
        let connector = connector(4, mode(800, 600, 60_000));
        let first_add = topology
            .reduce(
                KmsTopologyEvent::ConnectorScan(vec![connector.clone()]),
                &mut selector,
            )
            .expect("initial scan");
        let stale_generation = command_generation(&first_add[0]);
        topology
            .reduce(KmsTopologyEvent::UdevChange(Vec::new()), &mut selector)
            .expect("connector removal");
        let second_add = topology
            .reduce(
                KmsTopologyEvent::UdevChange(vec![connector.clone()]),
                &mut selector,
            )
            .expect("connector replug");
        let current_generation = command_generation(&second_add[0]);
        assert_ne!(current_generation, stale_generation);
        topology
            .reduce(
                KmsTopologyEvent::RenderReply(KmsRenderReply::OutputReady {
                    generation: current_generation,
                    key: connector.key.clone(),
                }),
                &mut selector,
            )
            .expect("current ready reply");
        assert_eq!(
            topology
                .output(connector.key.clone())
                .map(|output| (output.generation, output.phase)),
            Some((current_generation, OutputPhase::Ready))
        );

        topology
            .reduce(
                KmsTopologyEvent::RenderReply(KmsRenderReply::FrameSubmitted {
                    generation: stale_generation,
                    key: connector.key.clone(),
                }),
                &mut selector,
            )
            .expect("stale frame reply");

        assert_eq!(
            topology
                .output(connector.key.clone())
                .map(|output| (output.generation, output.frames_submitted)),
            Some((current_generation, 0))
        );
    }
}
