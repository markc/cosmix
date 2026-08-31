mod atomic_present;
#[cfg(any(all(feature = "kms-live", not(test)), test))]
mod atomic_presentation;
pub(crate) mod kms;
pub(crate) mod kms_live;
/// Present whenever the live session can exist, and under test so its policy is
/// checked offline. A default non-test build has no live session, so nothing in
/// here would be reachable and it is compiled out entirely rather than carried
/// as dead code.
#[cfg(any(feature = "kms-live", test))]
pub(crate) mod libinput_live;
pub(crate) mod probe;
pub(crate) mod render;
pub(crate) mod resume_scanout;
pub(crate) mod scan;
#[cfg(any(all(feature = "kms-live", not(test)), test))]
mod scanout_pool;
pub(crate) mod watch;
pub(crate) mod worker;

use smithay::{
    output::{Mode, Output},
    reexports::wayland_server::protocol::{wl_output::WlOutput, wl_surface::WlSurface},
};

#[cfg(any(all(feature = "kms-live", not(test)), test))]
use std::collections::BTreeMap;

#[cfg(any(all(feature = "kms-live", not(test)), test))]
use smithay::{
    output::{PhysicalProperties, Scale, Subpixel},
    reexports::wayland_server::{DisplayHandle, GlobalDispatch, backend::GlobalId},
    utils::Transform,
    wayland::output::WlOutputData,
};

use self::kms::{KmsRenderCommand, KmsRenderReply, KmsTopology, KmsTopologyError};

#[cfg(any(all(feature = "kms-live", not(test)), test))]
use self::kms::{OutputKey, SelectedOutput};

#[cfg(any(all(feature = "kms-live", not(test)), test))]
use self::kms::KmsTopologyLifecycleEvent;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BackendKind {
    Winit,
    Kms,
}

/// One rectangle of the seat's pointer space, in logical coordinates.
///
/// Half-open, matching `surface_at`: a point belongs to this region when
/// `x >= self.x && x < self.x + self.width`, and likewise vertically.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct SeatRegion {
    pub(crate) x: f64,
    pub(crate) y: f64,
    pub(crate) width: f64,
    pub(crate) height: f64,
}

#[cfg(feature = "bus")]
#[derive(Clone)]
pub(crate) struct PortOutputSource {
    pub(crate) output: Output,
    pub(crate) name: String,
    pub(crate) default: bool,
    pub(crate) x: i32,
    pub(crate) y: i32,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) scale: f64,
    pub(crate) refresh_mhz: u32,
}

pub(crate) enum BackendData {
    Winit(WinitBackendData),
    Kms(KmsBackendData),
}

pub(crate) struct WinitBackendData {
    pub(crate) output: Output,
    pub(crate) output_mode: Mode,
    pub(crate) output_size: (u32, u32),
    pub(crate) output_scale: f64,
}

/// Immutable output geometry captured when a screencopy frame object is made.
///
/// A later mode/topology change invalidates the frame instead of silently
/// changing the dimensions promised on the wire. `readback_supported` keeps
/// the S-1a KMS seam fail-closed until the S-1b scanout readback lands.
#[derive(Clone, Debug)]
pub(crate) struct CaptureSourceSnapshot {
    pub(crate) output_name: String,
    pub(crate) logical_rect: (i32, i32, u32, u32),
    pub(crate) physical_size: (u32, u32),
    pub(crate) scale: f64,
    pub(crate) transform: smithay::utils::Transform,
    pub(crate) generation: u64,
    pub(crate) readback_supported: bool,
}

pub(crate) struct KmsBackendData {
    topology: KmsTopology,
    /// Session-lifetime client outputs. Deliberately separate from
    /// `KmsTopology::outputs`, whose DRM-master-lifetime records are cleared on
    /// every pause.
    #[cfg(any(all(feature = "kms-live", not(test)), test))]
    client_outputs: Box<KmsClientOutputs>,
    output_size: (u32, u32),
    #[cfg(test)]
    output_enters: usize,
    #[cfg(test)]
    output_leaves: usize,
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
struct KmsClientOutput {
    selected: SelectedOutput,
    output: Output,
    global: GlobalId,
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
#[derive(Default)]
struct KmsClientOutputs {
    outputs: BTreeMap<OutputKey, KmsClientOutput>,
}

impl KmsBackendData {
    pub(crate) fn new(output_size: (u32, u32)) -> Self {
        Self {
            topology: KmsTopology::default(),
            #[cfg(any(all(feature = "kms-live", not(test)), test))]
            client_outputs: Box::default(),
            output_size,
            #[cfg(test)]
            output_enters: 0,
            #[cfg(test)]
            output_leaves: 0,
        }
    }
}

impl BackendData {
    /// The output used when a protocol request deliberately leaves output
    /// selection to the compositor.
    pub(crate) fn default_output(&self) -> Option<Output> {
        match self {
            Self::Winit(data) => Some(data.output.clone()),
            Self::Kms(data) => data.default_output(),
        }
    }

    /// Owned facts for the protocol-visible output tree.
    ///
    /// P-0 exposes one client output, but this returns a vector so the read
    /// surface does not need another backend seam when KMS grows multi-output
    /// client globals.
    #[cfg(feature = "bus")]
    pub(crate) fn port_outputs(&self) -> Vec<PortOutputSource> {
        let default = self.default_output();
        match self {
            Self::Winit(data) => std::iter::once(data)
                .filter_map(|data| {
                    Some(PortOutputSource {
                        output: data.output.clone(),
                        name: data.output.name(),
                        default: default.as_ref() == Some(&data.output),
                        x: 0,
                        y: 0,
                        width: data.output_size.0,
                        height: data.output_size.1,
                        scale: data.output_scale,
                        refresh_mhz: u32::try_from(data.output_mode.refresh).ok()?,
                    })
                })
                .collect(),
            Self::Kms(data) => {
                #[cfg(any(all(feature = "kms-live", not(test)), test))]
                {
                    data.client_outputs
                        .outputs
                        .values()
                        .filter_map(|source| {
                            let selected = &source.selected;
                            Some(PortOutputSource {
                                output: source.output.clone(),
                                name: selected.key.connector_name.clone(),
                                default: default.as_ref() == Some(&source.output),
                                x: selected.logical_rect.x,
                                y: selected.logical_rect.y,
                                width: u32::try_from(selected.logical_rect.width).ok()?,
                                height: u32::try_from(selected.logical_rect.height).ok()?,
                                scale: selected.output_scale.as_f64(),
                                refresh_mhz: selected.connector_mode.refresh_millihz,
                            })
                        })
                        .collect()
                }
                #[cfg(not(any(all(feature = "kms-live", not(test)), test)))]
                {
                    let _ = data;
                    Vec::new()
                }
            }
        }
    }

    /// Owned facts for one protocol-visible output without projecting the
    /// complete output set. Observation dirty hooks use this fixed-cost path.
    #[cfg(feature = "bus")]
    pub(crate) fn port_output(&self, requested: &Output) -> Option<PortOutputSource> {
        let default = self.default_output().as_ref() == Some(requested);
        match self {
            Self::Winit(data) if data.output == *requested => Some(PortOutputSource {
                output: data.output.clone(),
                name: data.output.name(),
                default,
                x: 0,
                y: 0,
                width: data.output_size.0,
                height: data.output_size.1,
                scale: data.output_scale,
                refresh_mhz: u32::try_from(data.output_mode.refresh).ok()?,
            }),
            Self::Winit(_) => None,
            Self::Kms(_) => {
                let mode = requested.current_mode()?;
                let location = requested.current_location();
                let scale = requested.current_scale().fractional_scale();
                let logical_size = mode.size.to_f64().to_logical(scale).to_i32_round::<i32>();
                let logical_size = requested.current_transform().transform_size(logical_size);
                Some(PortOutputSource {
                    output: requested.clone(),
                    name: requested.name(),
                    default,
                    x: location.x,
                    y: location.y,
                    width: u32::try_from(logical_size.w.max(0)).ok()?,
                    height: u32::try_from(logical_size.h.max(0)).ok()?,
                    scale,
                    refresh_mhz: u32::try_from(mode.refresh).ok()?,
                })
            }
        }
    }

    /// Resolve a client output only while it remains in this backend's live
    /// protocol registry. `Output::from_resource` alone also resolves disabled
    /// globals retained by existing client resources after hot-unplug.
    pub(crate) fn output_from_resource(&self, resource: &WlOutput) -> Option<Output> {
        Output::from_resource(resource).filter(|output| self.output_is_registered(output))
    }

    /// Resolve the exact client-selected output without a default-output
    /// fallback. S-1a reads nested output pixels; registered KMS outputs still
    /// advertise the protocol and snapshot truthful geometry, but fail a copy
    /// through `readback_supported` until S-1b supplies their final-output seam.
    pub(crate) fn capture_source_for_output(
        &self,
        resource: &WlOutput,
    ) -> Option<CaptureSourceSnapshot> {
        let output = self.output_from_resource(resource)?;
        match self {
            Self::Winit(data) if data.output == output => {
                let physical_width = (f64::from(data.output_size.0) * data.output_scale).round();
                let physical_height = (f64::from(data.output_size.1) * data.output_scale).round();
                Some(CaptureSourceSnapshot {
                    output_name: output.name(),
                    logical_rect: (0, 0, data.output_size.0, data.output_size.1),
                    physical_size: (
                        u32::try_from(physical_width as u64).ok()?,
                        u32::try_from(physical_height as u64).ok()?,
                    ),
                    scale: data.output_scale,
                    transform: smithay::utils::Transform::Normal,
                    generation: 1,
                    readback_supported: true,
                })
            }
            Self::Kms(data) => {
                #[cfg(any(all(feature = "kms-live", not(test)), test))]
                {
                    let mode = output.current_mode()?;
                    let location = output.current_location();
                    let scale = output.current_scale().fractional_scale();
                    let logical_width = (f64::from(mode.size.w) / scale).round();
                    let logical_height = (f64::from(mode.size.h) / scale).round();
                    let key = &data
                        .client_outputs
                        .outputs
                        .values()
                        .find(|registered| registered.output == output)?
                        .selected
                        .key;
                    let generation = *data.topology.presentation_generations().get(key)?;
                    Some(CaptureSourceSnapshot {
                        output_name: output.name(),
                        logical_rect: (
                            location.x,
                            location.y,
                            u32::try_from(logical_width as i64).ok()?,
                            u32::try_from(logical_height as i64).ok()?,
                        ),
                        physical_size: (
                            u32::try_from(mode.size.w).ok()?,
                            u32::try_from(mode.size.h).ok()?,
                        ),
                        scale,
                        transform: output.current_transform(),
                        generation,
                        readback_supported: false,
                    })
                }
                #[cfg(not(any(all(feature = "kms-live", not(test)), test)))]
                {
                    let _ = (data, output);
                    None
                }
            }
            Self::Winit(_) => None,
        }
    }

    pub(crate) fn output_is_registered(&self, output: &Output) -> bool {
        match self {
            Self::Winit(data) => data.output == *output,
            Self::Kms(data) => data.output_is_registered(output),
        }
    }

    #[cfg(any(all(feature = "kms-live", not(test)), test))]
    pub(crate) fn kms_registered_outputs(&self) -> Vec<(OutputKey, Output)> {
        match self {
            Self::Winit(_) => Vec::new(),
            Self::Kms(data) => data
                .client_outputs
                .outputs
                .iter()
                .map(|(key, output)| (key.clone(), output.output.clone()))
                .collect(),
        }
    }

    #[cfg(any(all(feature = "kms-live", not(test)), test))]
    pub(crate) fn kms_output_is_ready(&self, generation: u64, key: &self::kms::OutputKey) -> bool {
        match self {
            Self::Winit(_) => false,
            Self::Kms(data) => data.topology.output_is_ready(generation, key),
        }
    }

    #[cfg(any(all(feature = "kms-live", not(test)), test))]
    pub(crate) fn kms_presentation_generations(&self) -> BTreeMap<OutputKey, u64> {
        match self {
            Self::Winit(_) => BTreeMap::new(),
            Self::Kms(data) => data.topology.presentation_generations(),
        }
    }

    /// Perform backend-specific maintenance after one protocol dispatch.
    ///
    /// The nested backend owns one Smithay output that needs cleanup. The KMS
    /// backend has no equivalent output object until live outputs are admitted;
    /// its topology is maintained by explicit udev/session/render events.
    pub(crate) fn maintain_after_protocol_dispatch(&mut self) {
        match self {
            Self::Winit(data) => data.output.cleanup(),
            Self::Kms(data) => data.maintain_after_protocol_dispatch(),
        }
    }

    pub(crate) fn apply_kms_render_reply(
        &mut self,
        reply: KmsRenderReply,
    ) -> Result<Vec<KmsRenderCommand>, KmsRenderReplyApplyError> {
        let Self::Kms(data) = self else {
            return Err(KmsRenderReplyApplyError::WrongBackend);
        };
        data.apply_render_reply(reply)
            .map_err(KmsRenderReplyApplyError::Topology)
    }

    #[cfg(any(all(feature = "kms-live", not(test)), test))]
    pub(crate) fn apply_kms_topology_lifecycle(
        &mut self,
        event: KmsTopologyLifecycleEvent,
    ) -> Result<Vec<KmsRenderCommand>, KmsRenderReplyApplyError> {
        let Self::Kms(data) = self else {
            return Err(KmsRenderReplyApplyError::WrongBackend);
        };
        data.topology
            .reduce_lifecycle(event)
            .map_err(KmsRenderReplyApplyError::Topology)
    }

    /// Reconcile the one protocol-visible KMS output after a logical topology
    /// admission or active-session connector change.
    ///
    /// The caller deliberately does not invoke this for session pause: loss of
    /// DRM master is not logical hot-unplug, so both the Smithay output and its
    /// global stay alive. `mapped_surfaces` closes the race where a client maps
    /// before the initial topology command is admitted.
    #[cfg(any(all(feature = "kms-live", not(test)), test))]
    pub(crate) fn reconcile_kms_client_output<D>(
        &mut self,
        display: &DisplayHandle,
        mapped_surfaces: &[WlSurface],
    ) where
        D: GlobalDispatch<WlOutput, WlOutputData> + 'static,
    {
        let Self::Kms(data) = self else {
            return;
        };
        data.reconcile_client_output::<D>(display, mapped_surfaces);
    }

    pub(crate) fn output_size(&self) -> (u32, u32) {
        match self {
            Self::Winit(data) => data.output_size,
            Self::Kms(data) => data.output_size,
        }
    }

    /// Protocol layout extent in logical coordinates.
    ///
    /// KMS starts with an already admitted logical bootstrap extent and then
    /// reads the retained topology once it exists. Physical mode dimensions
    /// never cross this seam. Nested mode is unchanged because its host window
    /// is the compositor's logical space.
    pub(crate) fn logical_output_size(&self) -> (u32, u32) {
        match self {
            Self::Winit(data) => data.output_size,
            Self::Kms(data) => data
                .topology
                .selected_client_output()
                .and_then(|output| {
                    Some((
                        u32::try_from(output.logical_rect.width).ok()?,
                        u32::try_from(output.logical_rect.height).ok()?,
                    ))
                })
                .unwrap_or(data.output_size),
        }
    }

    pub(crate) fn logical_output_rect(&self) -> (i32, i32, u32, u32) {
        match self {
            Self::Winit(data) => (0, 0, data.output_size.0, data.output_size.1),
            Self::Kms(data) => data
                .topology
                .selected_client_output()
                .and_then(|output| {
                    Some((
                        output.logical_rect.x,
                        output.logical_rect.y,
                        u32::try_from(output.logical_rect.width).ok()?,
                        u32::try_from(output.logical_rect.height).ok()?,
                    ))
                })
                .unwrap_or((0, 0, data.output_size.0, data.output_size.1)),
        }
    }

    /// The coordinate space input is confined to and absolute devices are
    /// transformed into.
    ///
    /// Not `output_size`. On the nested backend the two are the same thing —
    /// the host window *is* the seat. On bare metal they are not: `output_size`
    /// holds the bootstrap extent the compositor starts at, and no KMS event
    /// ever updates it, while the outputs that actually exist live in the
    /// topology. Wiring input to `output_size` would confine a real seat to the
    /// startup placeholder and scale every absolute device into it.
    ///
    /// The fallback is the bootstrap extent, which is the honest answer before
    /// any output is admitted: there is no layout to confine to yet.
    ///
    /// This is a bounding box, so it is the right answer for scaling a
    /// normalised absolute device into the seat and the wrong answer for
    /// confinement — see `seat_regions`.
    pub(crate) fn seat_extent(&self) -> (u32, u32) {
        match self {
            Self::Winit(data) => data.output_size,
            Self::Kms(data) => data.topology.seat_extent().unwrap_or(data.output_size),
        }
    }

    /// The regions the pointer is allowed to occupy.
    ///
    /// Not the bounding box. Outputs of unequal height leave dead space inside
    /// their bounding box: 1920x1080 beside 1280x1024 bounds to 3200x1080, and
    /// the 1280x56 strip under the shorter output belongs to no display at all.
    /// Confining to the box would let the cursor sit somewhere nothing can draw
    /// it. Confinement therefore takes the union of the admitted rectangles,
    /// while `seat_extent` keeps the box for scaling absolute devices.
    ///
    /// Never empty: with no admitted output the bootstrap extent is the region,
    /// which keeps the pointer inside the space the compositor is rendering.
    pub(crate) fn seat_regions(&self) -> Vec<SeatRegion> {
        let bootstrap = |size: (u32, u32)| {
            vec![SeatRegion {
                x: 0.0,
                y: 0.0,
                width: f64::from(size.0),
                height: f64::from(size.1),
            }]
        };
        match self {
            Self::Winit(data) => bootstrap(data.output_size),
            Self::Kms(data) => {
                let regions = data.topology.seat_regions();
                if regions.is_empty() {
                    bootstrap(data.output_size)
                } else {
                    regions
                }
            }
        }
    }

    pub(crate) fn output_scale(&self) -> f64 {
        match self {
            Self::Winit(data) => data.output_scale,
            Self::Kms(data) => data.topology.output_scale().as_f64(),
        }
    }

    pub(crate) fn output_enter(&mut self, surface: &WlSurface) {
        match self {
            Self::Winit(data) => data.output.enter(surface),
            Self::Kms(data) => data.output_enter(surface),
        }
    }

    pub(crate) fn output_leave(&mut self, surface: &WlSurface) {
        match self {
            Self::Winit(data) => data.output.leave(surface),
            Self::Kms(data) => data.output_leave(surface),
        }
    }

    /// Apply a host-window resize. Bare-metal output modes are lifecycle
    /// events and therefore never change through this nested-only input.
    pub(crate) fn resize_host_output(&mut self, size: (u32, u32)) -> bool {
        match self {
            Self::Kms(_) => false,
            Self::Winit(data) => {
                if data.output_size == size {
                    return false;
                }
                data.output_size = size;
                let mode = output_mode(size);
                data.output.delete_mode(data.output_mode);
                data.output
                    .change_current_state(Some(mode), None, None, None);
                data.output.set_preferred(mode);
                data.output_mode = mode;
                true
            }
        }
    }

    /// Apply a host-window scale. Bare-metal scale comes from KMS output
    /// admission and is not mutable through a winit event.
    pub(crate) fn change_host_output_scale(&mut self, scale: f64) -> bool {
        match self {
            Self::Kms(_) => false,
            Self::Winit(data) => {
                if !scale.is_finite()
                    || scale <= 0.0
                    || (data.output_scale - scale).abs() < f64::EPSILON
                {
                    return false;
                }
                data.output_scale = scale;
                true
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn kms_output_transitions(&self) -> Option<(usize, usize)> {
        match self {
            Self::Winit(_) => None,
            Self::Kms(data) => Some((data.output_enters, data.output_leaves)),
        }
    }
}

impl KmsBackendData {
    fn default_output(&self) -> Option<Output> {
        #[cfg(any(all(feature = "kms-live", not(test)), test))]
        {
            if let Some(selected) = self.topology.selected_client_output()
                && let Some(output) = self.client_outputs.outputs.get(&selected.key)
            {
                return Some(output.output.clone());
            }
            self.client_outputs
                .outputs
                .values()
                .next()
                .map(|output| output.output.clone())
        }
        #[cfg(not(any(all(feature = "kms-live", not(test)), test)))]
        {
            None
        }
    }

    fn output_is_registered(&self, _output: &Output) -> bool {
        #[cfg(any(all(feature = "kms-live", not(test)), test))]
        {
            self.client_outputs
                .outputs
                .values()
                .any(|registered| registered.output == *_output)
        }
        #[cfg(not(any(all(feature = "kms-live", not(test)), test)))]
        {
            false
        }
    }

    fn apply_render_reply(
        &mut self,
        reply: KmsRenderReply,
    ) -> Result<Vec<KmsRenderCommand>, KmsTopologyError> {
        let mut no_selection = |_: &kms::ConnectorDescription,
                                _: kms::ConnectorMode|
         -> Result<kms::AtomicOutputSelection, String> {
            Err("render replies cannot request atomic output selection".to_string())
        };
        self.topology
            .reduce(kms::KmsTopologyEvent::RenderReply(reply), &mut no_selection)
    }

    fn maintain_after_protocol_dispatch(&mut self) {
        #[cfg(any(all(feature = "kms-live", not(test)), test))]
        for output in self.client_outputs.outputs.values() {
            output.output.cleanup();
        }
    }

    fn output_enter(&mut self, _surface: &WlSurface) {
        #[cfg(any(all(feature = "kms-live", not(test)), test))]
        for output in self.client_outputs.outputs.values() {
            output.output.enter(_surface);
        }
        #[cfg(test)]
        {
            self.output_enters += 1;
        }
    }

    fn output_leave(&mut self, _surface: &WlSurface) {
        #[cfg(any(all(feature = "kms-live", not(test)), test))]
        for output in self.client_outputs.outputs.values() {
            output.output.leave(_surface);
        }
        #[cfg(test)]
        {
            self.output_leaves += 1;
        }
    }

    #[cfg(any(all(feature = "kms-live", not(test)), test))]
    fn reconcile_client_output<D>(&mut self, display: &DisplayHandle, mapped_surfaces: &[WlSurface])
    where
        D: GlobalDispatch<WlOutput, WlOutputData> + 'static,
    {
        let selected = self.topology.selected_client_output().cloned();
        let selected_key = selected.as_ref().map(|output| &output.key);
        let removed = self
            .client_outputs
            .outputs
            .keys()
            .filter(|key| Some(*key) != selected_key)
            .cloned()
            .collect::<Vec<_>>();

        for key in removed {
            let output = self
                .client_outputs
                .outputs
                .remove(&key)
                .expect("removed KMS client output came from the registry");
            for surface in mapped_surfaces {
                output.output.leave(surface);
            }
            display.disable_global::<D>(output.global);
        }

        let Some(selected) = selected else {
            return;
        };
        if let Some(current) = self.client_outputs.outputs.get_mut(&selected.key) {
            if current.selected != selected {
                reconcile_output_state(&current.output, &current.selected, &selected);
                current.selected = selected;
            }
            for surface in mapped_surfaces {
                current.output.enter(surface);
            }
            return;
        }

        let output = Output::new(
            selected.key.connector_name.clone(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "CosMix".into(),
                model: selected.key.connector_name.clone(),
            },
        );
        configure_output_state(&output, &selected);
        let global = output.create_global::<D>(display);
        for surface in mapped_surfaces {
            output.enter(surface);
        }
        self.client_outputs.outputs.insert(
            selected.key.clone(),
            KmsClientOutput {
                selected,
                output,
                global,
            },
        );
    }
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn configure_output_state(output: &Output, selected: &SelectedOutput) {
    let mode = selected_output_mode(selected);
    output.set_preferred(mode);
    output.change_current_state(
        Some(mode),
        Some(Transform::Normal),
        Some(Scale::Fractional(selected.output_scale.as_f64())),
        Some((selected.logical_rect.x, selected.logical_rect.y).into()),
    );
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn reconcile_output_state(output: &Output, previous: &SelectedOutput, selected: &SelectedOutput) {
    let previous_mode = selected_output_mode(previous);
    let mode = selected_output_mode(selected);
    if previous_mode != mode {
        output.delete_mode(previous_mode);
    }
    output.set_preferred(mode);
    output.change_current_state(
        Some(mode),
        Some(Transform::Normal),
        Some(Scale::Fractional(selected.output_scale.as_f64())),
        Some((selected.logical_rect.x, selected.logical_rect.y).into()),
    );
}

#[cfg(any(all(feature = "kms-live", not(test)), test))]
fn selected_output_mode(selected: &SelectedOutput) -> Mode {
    Mode {
        size: (
            selected.connector_mode.width as i32,
            selected.connector_mode.height as i32,
        )
            .into(),
        refresh: selected.connector_mode.refresh_millihz as i32,
    }
}

#[derive(Debug)]
pub(crate) enum KmsRenderReplyApplyError {
    WrongBackend,
    Topology(KmsTopologyError),
}

impl std::fmt::Display for KmsRenderReplyApplyError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WrongBackend => formatter.write_str("KMS render reply reached the winit backend"),
            Self::Topology(error) => {
                write!(formatter, "KMS topology rejected render reply: {error}")
            }
        }
    }
}

impl std::error::Error for KmsRenderReplyApplyError {}

fn output_mode(size: (u32, u32)) -> Mode {
    Mode {
        size: (size.0 as i32, size.1 as i32).into(),
        refresh: 60_000,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kms_backend_reaches_protocol_loop_maintenance_without_winit_access() {
        let mut backend = BackendData::Kms(KmsBackendData::new((1920, 1080)));

        backend.maintain_after_protocol_dispatch();
        assert_eq!(backend.output_scale(), 1.0);

        let BackendData::Kms(kms) = backend else {
            panic!("KMS backend identity changed during loop maintenance");
        };
        assert_eq!(kms.topology.generation(), 0);
        assert_eq!(kms.output_size, (1920, 1080));
    }

    #[test]
    fn kms_seat_extent_falls_back_to_the_bootstrap_extent_before_any_output() {
        // Before an output is admitted there is no layout to confine input to,
        // and the bootstrap extent is the only honest answer. Once one is
        // admitted the topology answers instead — `KmsTopology::seat_extent`
        // owns that half and is tested against real modes in `kms.rs`.
        let backend = BackendData::Kms(KmsBackendData::new((960, 640)));

        assert_eq!(backend.seat_extent(), (960, 640));
        assert_eq!(
            backend.seat_extent(),
            backend.output_size(),
            "with no admitted output the two coincide; they diverge the moment one lands"
        );
    }

    #[cfg(feature = "bus")]
    #[test]
    fn winit_port_output_with_invalid_refresh_is_skipped() {
        let output = Output::new(
            "invalid-refresh".into(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "CosMix".into(),
                model: "Invalid refresh fixture".into(),
            },
        );
        let backend = BackendData::Winit(WinitBackendData {
            output,
            output_mode: Mode {
                size: (1920, 1080).into(),
                refresh: -1,
            },
            output_size: (1920, 1080),
            output_scale: 1.0,
        });

        assert!(backend.port_outputs().is_empty());
    }
}
