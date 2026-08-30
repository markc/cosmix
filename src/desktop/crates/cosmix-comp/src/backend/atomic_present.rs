//! Read-only atomic-KMS route and scanout-format admission.
//!
//! Live request construction and pageflip waiting live in the sibling
//! `atomic_presentation` module so this admission path stays getter-only.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{File, OpenOptions},
    io,
    os::{
        fd::{BorrowedFd, OwnedFd},
        unix::fs::OpenOptionsExt,
    },
    path::{Path, PathBuf},
};

#[cfg(test)]
use cosmix_wgpu_dmabuf::ScanoutWgpuFormat;
use cosmix_wgpu_dmabuf::{
    ManualVulkanRenderer, ScanoutImportCapabilities, ScanoutImportSupport,
    is_opaque_scanout_format, preferred_scanout_fourccs,
};
use drm_fourcc::{DrmFourcc, DrmModifier};
use smithay::{
    backend::allocator::{
        Buffer, Fourcc, Modifier,
        gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
    },
    reexports::drm::control::{self, PlaneType, ResourceHandle},
};

use super::{
    kms::{AtomicOutputSelection, ConnectorMode},
    scan::connector_mode,
};

const REQUIRED_CONNECTOR_PROPERTIES: &[&str] = &["CRTC_ID"];
const REQUIRED_CRTC_PROPERTIES: &[&str] = &["ACTIVE", "MODE_ID"];
const REQUIRED_PRIMARY_PLANE_PROPERTIES: &[&str] = &[
    "type",
    "CRTC_ID",
    "FB_ID",
    "CRTC_X",
    "CRTC_Y",
    "CRTC_W",
    "CRTC_H",
    "SRC_X",
    "SRC_Y",
    "SRC_W",
    "SRC_H",
    "IN_FORMATS",
];

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct AtomicFormatModifier {
    pub(crate) fourcc: u32,
    pub(crate) modifier: u64,
}

impl AtomicFormatModifier {
    pub(crate) fn format_name(self) -> String {
        DrmFourcc::try_from(self.fourcc)
            .map(|format| format!("{format:?}"))
            .unwrap_or_else(|_| format!("unknown-{:#010x}", self.fourcc))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum AtomicCapabilityState {
    Supported,
    Rejected(String),
}

impl AtomicCapabilityState {
    fn supported(&self) -> bool {
        matches!(self, Self::Supported)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AtomicCandidateRejection {
    pub(crate) candidate: AtomicFormatModifier,
    pub(crate) plane_in_formats: AtomicCapabilityState,
    pub(crate) gbm_allocation: AtomicCapabilityState,
    pub(crate) vulkan_colour_attachment: AtomicCapabilityState,
    pub(crate) wgpu_render_attachment: AtomicCapabilityState,
    pub(crate) selection_policy: AtomicCapabilityState,
}

impl AtomicCandidateRejection {
    fn admitted(&self) -> bool {
        self.plane_in_formats.supported()
            && self.gbm_allocation.supported()
            && self.vulkan_colour_attachment.supported()
            && self.wgpu_render_attachment.supported()
            && self.selection_policy.supported()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AtomicRejectionMatrix {
    pub(crate) route_rejection: Option<String>,
    pub(crate) candidates: Vec<AtomicCandidateRejection>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum AtomicAdmissionOutcome {
    Selected(AtomicSelectedAdmission),
    Rejected(AtomicRejectionMatrix),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AtomicSelectedAdmission {
    pub(crate) selection: AtomicOutputSelection,
    pub(crate) total_candidates: usize,
    pub(crate) evaluated_candidates: usize,
    pub(crate) unevaluated_candidates: usize,
    pub(crate) other_admissible_survivors: AtomicSurvivorStatus,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AtomicSurvivorStatus {
    None,
    UnknownNotEvaluated,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AtomicConnectorAdmission {
    pub(crate) connector_name: String,
    pub(crate) connector_id: u32,
    pub(crate) outcome: AtomicAdmissionOutcome,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct AtomicAdmissionReport {
    pub(crate) device_path: PathBuf,
    pub(crate) device_opened: bool,
    pub(crate) connectors: Vec<AtomicConnectorAdmission>,
    pub(crate) error: Option<String>,
}

impl AtomicAdmissionReport {
    pub(crate) fn selected_count(&self) -> usize {
        self.connectors
            .iter()
            .filter(|connector| matches!(connector.outcome, AtomicAdmissionOutcome::Selected(_)))
            .count()
    }

    pub(crate) fn setup_succeeded(&self) -> bool {
        self.device_opened && self.error.is_none()
    }
}

#[derive(Clone, Debug, Default)]
struct AtomicPropertyTable(BTreeMap<String, u64>);

impl AtomicPropertyTable {
    fn missing<'a>(&self, required: &[&'a str]) -> Vec<&'a str> {
        required
            .iter()
            .copied()
            .filter(|name| !self.0.contains_key(*name))
            .collect()
    }

    fn get(&self, name: &str) -> Option<u64> {
        self.0.get(name).copied()
    }
}

#[derive(Clone, Debug)]
struct EnumeratedConnector {
    id: u32,
    name: String,
    connected: bool,
    modes: Vec<ConnectorMode>,
    possible_crtcs: BTreeSet<u32>,
    properties: AtomicPropertyTable,
}

#[derive(Clone, Debug)]
struct EnumeratedCrtc {
    id: u32,
    properties: AtomicPropertyTable,
}

#[derive(Clone, Debug)]
struct EnumeratedPlane {
    id: u32,
    possible_crtcs: BTreeSet<u32>,
    properties: AtomicPropertyTable,
}

#[derive(Clone, Debug, Default)]
struct AtomicEnumeration {
    connectors: Vec<EnumeratedConnector>,
    crtcs: Vec<EnumeratedCrtc>,
    planes: Vec<EnumeratedPlane>,
    blobs: BTreeMap<u64, Vec<u8>>,
}

trait AtomicCapabilityOracle {
    fn vulkan_modifiers(&self, fourcc: u32) -> Result<Vec<u64>, String>;
    fn scanout_support(
        &self,
        candidate: AtomicFormatModifier,
        width: u32,
        height: u32,
    ) -> Result<ScanoutImportSupport, String>;
    fn gbm_support(
        &mut self,
        width: u32,
        height: u32,
        candidate: AtomicFormatModifier,
    ) -> AtomicCapabilityState;
}

struct ProductionCapabilityOracle {
    scanout: ScanoutImportCapabilities,
    gbm: GbmAllocator<OwnedFd>,
}

impl AtomicCapabilityOracle for ProductionCapabilityOracle {
    fn vulkan_modifiers(&self, fourcc: u32) -> Result<Vec<u64>, String> {
        self.scanout
            .modifiers_for(fourcc)
            .map_err(|error| error.to_string())
    }

    fn scanout_support(
        &self,
        candidate: AtomicFormatModifier,
        width: u32,
        height: u32,
    ) -> Result<ScanoutImportSupport, String> {
        self.scanout
            .query(candidate.fourcc, candidate.modifier, width, height)
            .map_err(|error| error.to_string())
    }

    fn gbm_support(
        &mut self,
        width: u32,
        height: u32,
        candidate: AtomicFormatModifier,
    ) -> AtomicCapabilityState {
        let Ok(fourcc) = Fourcc::try_from(candidate.fourcc) else {
            return AtomicCapabilityState::Rejected("GBM does not recognise the fourcc".into());
        };
        let requested = Modifier::from(candidate.modifier);
        match self.gbm.create_buffer_with_flags(
            width,
            height,
            fourcc,
            &[requested],
            GbmBufferFlags::SCANOUT | GbmBufferFlags::RENDERING,
        ) {
            Ok(buffer) if Buffer::format(&buffer).modifier == requested => {
                AtomicCapabilityState::Supported
            }
            Ok(buffer) => AtomicCapabilityState::Rejected(format!(
                "GBM returned modifier {:#018x} instead of requested {:#018x}",
                u64::from(Buffer::format(&buffer).modifier),
                candidate.modifier
            )),
            Err(error) => AtomicCapabilityState::Rejected(error.to_string()),
        }
    }
}

mod read_only_drm {
    use std::{
        fs::File,
        io,
        os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd},
    };

    use smithay::reexports::drm::{
        self,
        control::{self, Device as ControlDevice, ResourceHandle},
    };

    /// Read-only-by-construction DRM admission handle.
    ///
    /// Only the getter surface required by Rung 1 is delegated. The private
    /// inner type implements the broad drm traits because that crate provides
    /// getters through them, but callers cannot name or borrow it and therefore
    /// cannot reach master, framebuffer, property-set, or commit methods.
    #[derive(Debug)]
    pub(super) struct ReadOnlyCard {
        inner: GetterDevice,
    }

    #[derive(Debug)]
    struct GetterDevice(File);

    impl AsFd for GetterDevice {
        fn as_fd(&self) -> BorrowedFd<'_> {
            self.0.as_fd()
        }
    }

    impl drm::Device for GetterDevice {}
    impl ControlDevice for GetterDevice {}

    impl ReadOnlyCard {
        pub(super) fn new(file: File) -> Self {
            Self {
                inner: GetterDevice(file),
            }
        }

        pub(super) fn try_clone_fd(&self) -> io::Result<OwnedFd> {
            self.inner.0.try_clone().map(Into::into)
        }

        pub(super) fn device_id(&self) -> io::Result<u64> {
            let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
            // SAFETY: `stat` is valid writable storage and is inspected only
            // after fstat reports successful initialisation.
            if unsafe { libc::fstat(self.inner.0.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(unsafe { stat.assume_init() }.st_rdev)
        }

        pub(super) fn enable_atomic_getters(&self) -> io::Result<()> {
            drm::Device::set_client_capability(
                &self.inner,
                drm::ClientCapability::UniversalPlanes,
                true,
            )?;
            drm::Device::set_client_capability(&self.inner, drm::ClientCapability::Atomic, true)
        }

        pub(super) fn resource_handles(&self) -> io::Result<control::ResourceHandles> {
            self.inner.resource_handles()
        }

        pub(super) fn plane_handles(&self) -> io::Result<Vec<control::plane::Handle>> {
            self.inner.plane_handles()
        }

        pub(super) fn connector(
            &self,
            handle: control::connector::Handle,
        ) -> io::Result<control::connector::Info> {
            self.inner.get_connector(handle, false)
        }

        pub(super) fn encoder(
            &self,
            handle: control::encoder::Handle,
        ) -> io::Result<control::encoder::Info> {
            self.inner.get_encoder(handle)
        }

        pub(super) fn plane(
            &self,
            handle: control::plane::Handle,
        ) -> io::Result<control::plane::Info> {
            self.inner.get_plane(handle)
        }

        pub(super) fn property_entries<T: ResourceHandle>(
            &self,
            handle: T,
        ) -> io::Result<Vec<(String, u64)>> {
            self.inner
                .get_properties(handle)?
                .iter()
                .map(|(property, value)| {
                    self.inner
                        .get_property(*property)
                        .map(|info| (info.name().to_string_lossy().into_owned(), *value))
                })
                .collect()
        }

        pub(super) fn property_blob(&self, blob: u64) -> io::Result<Vec<u8>> {
            self.inner.get_property_blob(blob)
        }
    }
}

use read_only_drm::ReadOnlyCard;

pub(crate) fn probe_atomic_admission(path: &Path, drm_device: u64) -> AtomicAdmissionReport {
    let mut report = AtomicAdmissionReport {
        device_path: path.to_path_buf(),
        ..AtomicAdmissionReport::default()
    };
    let card_file = match OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_CLOEXEC)
        .open(path)
    {
        Ok(file) => file,
        Err(error) => {
            report.error = Some(format!("atomic DRM enumeration open failed: {error}"));
            return report;
        }
    };
    report.device_opened = true;
    let card = ReadOnlyCard::new(card_file);
    let opened_device = match card.device_id() {
        Ok(device) => device,
        Err(error) => {
            report.error = Some(format!("atomic DRM opened-fd identity failed: {error}"));
            return report;
        }
    };
    if let Err(error) = validate_opened_device(drm_device, opened_device) {
        report.error = Some(error);
        return report;
    }
    let gbm_fd: OwnedFd = match card.try_clone_fd() {
        Ok(fd) => fd,
        Err(error) => {
            report.error = Some(format!("atomic GBM fd duplication failed: {error}"));
            return report;
        }
    };
    let enumeration = match enumerate_atomic_device(&card) {
        Ok(enumeration) => enumeration,
        Err(error) => {
            report.error = Some(format!("atomic DRM enumeration failed: {error}"));
            return report;
        }
    };
    let renderer = match ManualVulkanRenderer::new_for_drm_scanout_probe(drm_device) {
        Ok(renderer) => renderer,
        Err(error) => {
            report.error = Some(format!("atomic Vulkan capability setup failed: {error}"));
            return report;
        }
    };
    let gbm = match GbmDevice::new(gbm_fd) {
        Ok(device) => {
            GbmAllocator::new(device, GbmBufferFlags::SCANOUT | GbmBufferFlags::RENDERING)
        }
        Err(error) => {
            report.error = Some(format!("atomic GBM capability setup failed: {error}"));
            return report;
        }
    };
    let mut capabilities = ProductionCapabilityOracle {
        scanout: renderer.scanout_import_capabilities(),
        gbm,
    };
    report.connectors = negotiate_atomic_outputs(&enumeration, &mut capabilities);
    report
}

/// Run the same read-only admission against an already-authorised live DRM
/// descriptor. This changes client capability bits for getters, allocates only
/// temporary GBM probe buffers, and performs no framebuffer or KMS write.
#[cfg_attr(not(all(feature = "kms-live", not(test))), allow(dead_code))]
pub(crate) fn admit_atomic_output_from_fd(
    drm_fd: BorrowedFd<'_>,
    drm_device: u64,
    connector_id: u32,
    connector_name: &str,
    scanout: ScanoutImportCapabilities,
) -> Result<AtomicOutputSelection, String> {
    let card_fd = drm_fd
        .try_clone_to_owned()
        .map_err(|error| format!("atomic live admission fd duplication failed: {error}"))?;
    let card = ReadOnlyCard::new(File::from(card_fd));
    let opened_device = card
        .device_id()
        .map_err(|error| format!("atomic live admission fd identity failed: {error}"))?;
    validate_opened_device(drm_device, opened_device)?;
    let gbm_fd = card
        .try_clone_fd()
        .map_err(|error| format!("atomic live admission GBM fd duplication failed: {error}"))?;
    let enumeration = enumerate_atomic_device(&card)
        .map_err(|error| format!("atomic live DRM enumeration failed: {error}"))?;
    let gbm = GbmDevice::new(gbm_fd)
        .map_err(|error| format!("atomic live GBM capability setup failed: {error}"))?;
    let mut capabilities = ProductionCapabilityOracle {
        scanout,
        gbm: GbmAllocator::new(gbm, GbmBufferFlags::SCANOUT | GbmBufferFlags::RENDERING),
    };
    let admissions = negotiate_atomic_outputs(&enumeration, &mut capabilities);
    let admission = admissions
        .into_iter()
        .find(|admission| {
            admission.connector_id == connector_id && admission.connector_name == connector_name
        })
        .ok_or_else(|| {
            format!(
                "atomic live admission found no connector {connector_name} object {connector_id}"
            )
        })?;
    match admission.outcome {
        AtomicAdmissionOutcome::Selected(selected) => Ok(selected.selection),
        AtomicAdmissionOutcome::Rejected(matrix) => Err(format!(
            "atomic live admission rejected {connector_name} object {connector_id}: {matrix:?}"
        )),
    }
}

fn enumerate_atomic_device(card: &ReadOnlyCard) -> io::Result<AtomicEnumeration> {
    card.enable_atomic_getters()?;
    let resources = card.resource_handles()?;
    let mut blobs = BTreeMap::new();
    let connectors = resources
        .connectors()
        .iter()
        .map(|handle| -> io::Result<EnumeratedConnector> {
            let info = card.connector(*handle)?;
            let possible_crtcs = info
                .encoders()
                .iter()
                .map(|encoder| card.encoder(*encoder))
                .collect::<io::Result<Vec<_>>>()?
                .into_iter()
                .flat_map(|encoder| resources.filter_crtcs(encoder.possible_crtcs()))
                .map(u32::from)
                .collect();
            Ok(EnumeratedConnector {
                id: u32::from(*handle),
                name: info.to_string(),
                connected: info.state() == control::connector::State::Connected,
                modes: info.modes().iter().map(connector_mode).collect(),
                possible_crtcs,
                properties: properties(card, *handle)?,
            })
        })
        .collect::<io::Result<Vec<_>>>()?;
    let crtcs = resources
        .crtcs()
        .iter()
        .map(|handle| {
            Ok(EnumeratedCrtc {
                id: u32::from(*handle),
                properties: properties(card, *handle)?,
            })
        })
        .collect::<io::Result<Vec<_>>>()?;
    let planes = card
        .plane_handles()?
        .into_iter()
        .map(|handle| -> io::Result<EnumeratedPlane> {
            let info = card.plane(handle)?;
            let properties = properties(card, handle)?;
            if let Some(blob) = properties.get("IN_FORMATS")
                && blob != 0
                && !blobs.contains_key(&blob)
            {
                blobs.insert(blob, card.property_blob(blob)?);
            }
            Ok(EnumeratedPlane {
                id: u32::from(handle),
                possible_crtcs: resources
                    .filter_crtcs(info.possible_crtcs())
                    .into_iter()
                    .map(u32::from)
                    .collect(),
                properties,
            })
        })
        .collect::<io::Result<Vec<_>>>()?;
    Ok(AtomicEnumeration {
        connectors,
        crtcs,
        planes,
        blobs,
    })
}

fn properties<T: ResourceHandle>(
    card: &ReadOnlyCard,
    handle: T,
) -> io::Result<AtomicPropertyTable> {
    Ok(AtomicPropertyTable(
        card.property_entries(handle)?.into_iter().collect(),
    ))
}

fn validate_opened_device(expected: u64, actual: u64) -> Result<(), String> {
    if expected == actual {
        Ok(())
    } else {
        Err(format!(
            "atomic DRM opened-fd identity changed: expected dev_t {expected}, got {actual}"
        ))
    }
}

fn negotiate_atomic_outputs(
    enumeration: &AtomicEnumeration,
    capabilities: &mut dyn AtomicCapabilityOracle,
) -> Vec<AtomicConnectorAdmission> {
    enumeration
        .connectors
        .iter()
        .map(|connector| negotiate_connector(enumeration, connector, capabilities))
        .collect()
}

fn negotiate_connector(
    enumeration: &AtomicEnumeration,
    connector: &EnumeratedConnector,
    capabilities: &mut dyn AtomicCapabilityOracle,
) -> AtomicConnectorAdmission {
    let rejected = |reason: String| AtomicConnectorAdmission {
        connector_name: connector.name.clone(),
        connector_id: connector.id,
        outcome: AtomicAdmissionOutcome::Rejected(AtomicRejectionMatrix {
            route_rejection: Some(reason),
            candidates: Vec::new(),
        }),
    };
    if !connector.connected {
        return rejected("connector is not connected".into());
    }
    let Some(mode) = preferred_mode(&connector.modes) else {
        return rejected("connected connector exposes no modes".into());
    };
    let missing = connector.properties.missing(REQUIRED_CONNECTOR_PROPERTIES);
    if !missing.is_empty() {
        return rejected(format!(
            "connector {} missing atomic properties: {}",
            connector.id,
            missing.join(", ")
        ));
    }
    let current_crtc = connector
        .properties
        .get("CRTC_ID")
        .and_then(nonzero_u32)
        .filter(|crtc| connector.possible_crtcs.contains(crtc));
    let crtc_id = match current_crtc {
        Some(crtc) => crtc,
        None if connector.possible_crtcs.len() == 1 => *connector
            .possible_crtcs
            .first()
            .expect("one possible CRTC exists"),
        None if connector.possible_crtcs.is_empty() => {
            return rejected("connector has no compatible CRTC route".into());
        }
        None => {
            return rejected(format!(
                "connector route is ambiguous across CRTCs {:?}",
                connector.possible_crtcs
            ));
        }
    };
    let Some(crtc) = enumeration.crtcs.iter().find(|crtc| crtc.id == crtc_id) else {
        return rejected(format!(
            "selected CRTC {crtc_id} disappeared during enumeration"
        ));
    };
    let missing = crtc.properties.missing(REQUIRED_CRTC_PROPERTIES);
    if !missing.is_empty() {
        return rejected(format!(
            "CRTC {crtc_id} missing atomic properties: {}",
            missing.join(", ")
        ));
    }
    let mut primary_planes = enumeration
        .planes
        .iter()
        .filter(|plane| {
            plane.properties.get("type") == Some(PlaneType::Primary as u64)
                && plane.possible_crtcs.contains(&crtc_id)
        })
        .collect::<Vec<_>>();
    let attached = primary_planes
        .iter()
        .copied()
        .filter(|plane| plane.properties.get("CRTC_ID").and_then(nonzero_u32) == Some(crtc_id))
        .collect::<Vec<_>>();
    if attached.len() == 1 {
        primary_planes = attached;
    }
    let plane = match primary_planes.as_slice() {
        [] => return rejected(format!("CRTC {crtc_id} has no compatible primary plane")),
        [plane] => *plane,
        planes => {
            return rejected(format!(
                "CRTC {crtc_id} has ambiguous primary planes {:?}",
                planes.iter().map(|plane| plane.id).collect::<Vec<_>>()
            ));
        }
    };
    let missing = plane.properties.missing(REQUIRED_PRIMARY_PLANE_PROPERTIES);
    if !missing.is_empty() {
        return rejected(format!(
            "primary plane {} missing atomic properties: {}",
            plane.id,
            missing.join(", ")
        ));
    }
    let Some(blob_id) = plane.properties.get("IN_FORMATS").filter(|blob| *blob != 0) else {
        return rejected(format!(
            "primary plane {} has an empty IN_FORMATS property",
            plane.id
        ));
    };
    let Some(blob) = enumeration.blobs.get(&blob_id) else {
        return rejected(format!(
            "primary plane {} IN_FORMATS blob {blob_id} is unavailable",
            plane.id
        ));
    };
    let plane_formats = match decode_in_formats(blob) {
        Ok(formats) => formats,
        Err(error) => {
            return rejected(format!(
                "primary plane {} IN_FORMATS is invalid: {error}",
                plane.id
            ));
        }
    };
    let mut candidates = plane_formats.clone();
    for fourcc in preferred_scanout_fourccs() {
        if let Ok(modifiers) = capabilities.vulkan_modifiers(fourcc) {
            candidates.extend(
                modifiers
                    .into_iter()
                    .map(|modifier| AtomicFormatModifier { fourcc, modifier }),
            );
        }
    }
    let mut candidates = candidates.into_iter().collect::<Vec<_>>();
    candidates.sort_by_key(|candidate| candidate_rank(*candidate));
    let total_candidates = candidates.len();
    let mut matrix = Vec::with_capacity(total_candidates);
    // Admission stops at the first winner to avoid one mode-sized GBM
    // allocation for every later candidate. Individual driver ioctls remain
    // non-cancellable; Rung 1 cannot manufacture a truthful deadline for them.
    for (index, candidate) in candidates.into_iter().enumerate() {
        let row = evaluate_candidate(capabilities, &plane_formats, mode, candidate);
        if row.admitted() {
            let evaluated_candidates = index + 1;
            let unevaluated_candidates = total_candidates - evaluated_candidates;
            return AtomicConnectorAdmission {
                connector_name: connector.name.clone(),
                connector_id: connector.id,
                outcome: AtomicAdmissionOutcome::Selected(AtomicSelectedAdmission {
                    selection: AtomicOutputSelection {
                        connector_id: connector.id,
                        crtc_id,
                        primary_plane_id: plane.id,
                        mode,
                        format: candidate.fourcc,
                        modifier: candidate.modifier,
                    },
                    total_candidates,
                    evaluated_candidates,
                    unevaluated_candidates,
                    other_admissible_survivors: if unevaluated_candidates == 0 {
                        AtomicSurvivorStatus::None
                    } else {
                        AtomicSurvivorStatus::UnknownNotEvaluated
                    },
                }),
            };
        }
        matrix.push(row);
    }
    AtomicConnectorAdmission {
        connector_name: connector.name.clone(),
        connector_id: connector.id,
        outcome: AtomicAdmissionOutcome::Rejected(AtomicRejectionMatrix {
            route_rejection: None,
            candidates: matrix,
        }),
    }
}

fn evaluate_candidate(
    capabilities: &mut dyn AtomicCapabilityOracle,
    plane_formats: &BTreeSet<AtomicFormatModifier>,
    mode: ConnectorMode,
    candidate: AtomicFormatModifier,
) -> AtomicCandidateRejection {
    let scanout = capabilities.scanout_support(candidate, mode.width, mode.height);
    AtomicCandidateRejection {
        candidate,
        plane_in_formats: if plane_formats.contains(&candidate) {
            AtomicCapabilityState::Supported
        } else {
            AtomicCapabilityState::Rejected("absent from plane IN_FORMATS".into())
        },
        gbm_allocation: capabilities.gbm_support(mode.width, mode.height, candidate),
        vulkan_colour_attachment: match &scanout {
            Ok(support) if !support.vulkan_external_memory_colour_attachment => {
                AtomicCapabilityState::Rejected(
                    "Vulkan external-memory COLOR_ATTACHMENT import rejected".into(),
                )
            }
            Ok(support) if !support.vulkan_external_memory_transfer_src => {
                AtomicCapabilityState::Rejected(
                    "Vulkan external-memory TRANSFER_SRC usage rejected".into(),
                )
            }
            Ok(support) if !support.mode_extent_supported => {
                let maximum = support.max_extent.map_or_else(
                    || "unknown".into(),
                    |(width, height)| format!("{width}x{height}"),
                );
                AtomicCapabilityState::Rejected(format!(
                    "mode {}x{} exceeds Vulkan maxExtent {maximum}",
                    mode.width, mode.height
                ))
            }
            Ok(_) => AtomicCapabilityState::Supported,
            Err(error) => AtomicCapabilityState::Rejected(error.clone()),
        },
        wgpu_render_attachment: match &scanout {
            Ok(ScanoutImportSupport {
                wgpu_format: Some(_),
                ..
            }) => AtomicCapabilityState::Supported,
            Ok(_) => {
                AtomicCapabilityState::Rejected("no wgpu RENDER_ATTACHMENT format mapping".into())
            }
            Err(error) => AtomicCapabilityState::Rejected(error.clone()),
        },
        selection_policy: if is_opaque_scanout_format(candidate.fourcc) {
            AtomicCapabilityState::Supported
        } else {
            AtomicCapabilityState::Rejected(
                "policy requires an opaque XRGB8888 or XBGR8888 primary-plane format".into(),
            )
        },
    }
}

fn preferred_mode(modes: &[ConnectorMode]) -> Option<ConnectorMode> {
    modes.iter().copied().max_by_key(|mode| {
        (
            mode.preferred,
            u64::from(mode.width) * u64::from(mode.height),
            mode.refresh_millihz,
        )
    })
}

fn candidate_rank(candidate: AtomicFormatModifier) -> (u8, bool, u64) {
    // Deterministic provisional policy: opaque format rank, then non-linear,
    // then lowest raw modifier value. Revisit tiling preference before Rung 2
    // allocates real scanout buffers; raw modifier ordering is not performance
    // ordering.
    let format_rank = match DrmFourcc::try_from(candidate.fourcc) {
        Ok(DrmFourcc::Xrgb8888) => 0,
        Ok(DrmFourcc::Xbgr8888) => 1,
        _ => 2,
    };
    (
        format_rank,
        candidate.modifier == u64::from(DrmModifier::Linear),
        candidate.modifier,
    )
}

fn nonzero_u32(value: u64) -> Option<u32> {
    u32::try_from(value).ok().filter(|value| *value != 0)
}

fn decode_in_formats(blob: &[u8]) -> Result<BTreeSet<AtomicFormatModifier>, String> {
    const HEADER_SIZE: usize = 24;
    const MODIFIER_SIZE: usize = 24;
    if blob.len() < HEADER_SIZE {
        return Err("blob is shorter than drm_format_modifier_blob".into());
    }
    let version = read_u32(blob, 0)?;
    if version != 1 {
        return Err(format!("unsupported blob version {version}"));
    }
    let count_formats = usize::try_from(read_u32(blob, 8)?).map_err(|_| "format count overflow")?;
    let formats_offset =
        usize::try_from(read_u32(blob, 12)?).map_err(|_| "format offset overflow")?;
    let count_modifiers =
        usize::try_from(read_u32(blob, 16)?).map_err(|_| "modifier count overflow")?;
    let modifiers_offset =
        usize::try_from(read_u32(blob, 20)?).map_err(|_| "modifier offset overflow")?;
    let formats_end = formats_offset
        .checked_add(
            count_formats
                .checked_mul(4)
                .ok_or("format table overflow")?,
        )
        .ok_or("format table overflow")?;
    let modifiers_end = modifiers_offset
        .checked_add(
            count_modifiers
                .checked_mul(MODIFIER_SIZE)
                .ok_or("modifier table overflow")?,
        )
        .ok_or("modifier table overflow")?;
    if formats_offset < HEADER_SIZE || modifiers_offset < HEADER_SIZE {
        return Err("blob table starts inside drm_format_modifier_blob header".into());
    }
    if formats_end > blob.len() || modifiers_end > blob.len() {
        return Err("blob tables extend past the payload".into());
    }
    if ranges_overlap(formats_offset, formats_end, modifiers_offset, modifiers_end) {
        return Err("format and modifier tables overlap".into());
    }
    let formats = (0..count_formats)
        .map(|index| read_u32(blob, formats_offset + index * 4))
        .collect::<Result<Vec<_>, _>>()?;
    let mut decoded = BTreeSet::new();
    for index in 0..count_modifiers {
        let offset = modifiers_offset + index * MODIFIER_SIZE;
        let mask = read_u64(blob, offset)?;
        let format_offset = usize::try_from(read_u32(blob, offset + 8)?)
            .map_err(|_| "modifier format offset overflow")?;
        let modifier = read_u64(blob, offset + 16)?;
        for bit in 0..64 {
            if mask & (1_u64 << bit) == 0 {
                continue;
            }
            let format_index = checked_format_index(format_offset, bit)?;
            let Some(fourcc) = formats.get(format_index) else {
                return Err(format!(
                    "modifier references missing format index {format_index}"
                ));
            };
            decoded.insert(AtomicFormatModifier {
                fourcc: *fourcc,
                modifier,
            });
        }
    }
    Ok(decoded)
}

fn ranges_overlap(
    first_start: usize,
    first_end: usize,
    second_start: usize,
    second_end: usize,
) -> bool {
    first_start < second_end && second_start < first_end
}

fn checked_format_index(format_offset: usize, bit: usize) -> Result<usize, String> {
    format_offset
        .checked_add(bit)
        .ok_or_else(|| "modifier format index overflow".into())
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, String> {
    let value = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| "truncated u32".to_string())?;
    Ok(u32::from_ne_bytes(
        value.try_into().expect("four-byte slice"),
    ))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, String> {
    let value = bytes
        .get(offset..offset + 8)
        .ok_or_else(|| "truncated u64".to_string())?;
    Ok(u64::from_ne_bytes(
        value.try_into().expect("eight-byte slice"),
    ))
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    const ARGB: u32 = DrmFourcc::Argb8888 as u32;
    const XRGB: u32 = DrmFourcc::Xrgb8888 as u32;
    const XBGR: u32 = DrmFourcc::Xbgr8888 as u32;
    const LINEAR: u64 = 0;
    const TILED: u64 = 0x0100_0000_0000_0002;

    #[derive(Default)]
    struct FakeCapabilities {
        vulkan_modifiers: BTreeMap<u32, Vec<u64>>,
        scanout: BTreeMap<AtomicFormatModifier, ScanoutImportSupport>,
        gbm: BTreeSet<AtomicFormatModifier>,
        gbm_queries: usize,
        last_scanout_size: Cell<Option<(u32, u32)>>,
    }

    impl AtomicCapabilityOracle for FakeCapabilities {
        fn vulkan_modifiers(&self, fourcc: u32) -> Result<Vec<u64>, String> {
            Ok(self
                .vulkan_modifiers
                .get(&fourcc)
                .cloned()
                .unwrap_or_default())
        }

        fn scanout_support(
            &self,
            candidate: AtomicFormatModifier,
            width: u32,
            height: u32,
        ) -> Result<ScanoutImportSupport, String> {
            self.last_scanout_size.set(Some((width, height)));
            Ok(self
                .scanout
                .get(&candidate)
                .copied()
                .unwrap_or(ScanoutImportSupport {
                    wgpu_format: None,
                    vulkan_external_memory_colour_attachment: false,
                    vulkan_external_memory_transfer_src: false,
                    mode_extent_supported: false,
                    max_extent: None,
                }))
        }

        fn gbm_support(
            &mut self,
            _width: u32,
            _height: u32,
            candidate: AtomicFormatModifier,
        ) -> AtomicCapabilityState {
            self.gbm_queries += 1;
            if self.gbm.contains(&candidate) {
                AtomicCapabilityState::Supported
            } else {
                AtomicCapabilityState::Rejected("fake GBM rejection".into())
            }
        }
    }

    fn mode() -> ConnectorMode {
        ConnectorMode {
            width: 1920,
            height: 1080,
            refresh_millihz: 60_000,
            preferred: true,
            clock_khz: 148_500,
            hsync: (2008, 2052, 2200),
            vsync: (1084, 1089, 1125),
            hskew: 0,
            vscan: 0,
            flags: 0,
        }
    }

    fn properties(names: &[&str]) -> AtomicPropertyTable {
        AtomicPropertyTable(
            names
                .iter()
                .enumerate()
                .map(|(index, name)| ((*name).into(), u64::try_from(index + 1).unwrap()))
                .collect(),
        )
    }

    fn enumeration(
        connector_id: u32,
        crtc_id: u32,
        plane_id: u32,
        blob: Vec<u8>,
    ) -> AtomicEnumeration {
        let mut connector_properties = properties(REQUIRED_CONNECTOR_PROPERTIES);
        connector_properties
            .0
            .insert("CRTC_ID".into(), u64::from(crtc_id));
        let mut plane_properties = properties(REQUIRED_PRIMARY_PLANE_PROPERTIES);
        plane_properties
            .0
            .insert("type".into(), PlaneType::Primary as u64);
        plane_properties
            .0
            .insert("CRTC_ID".into(), u64::from(crtc_id));
        plane_properties.0.insert("IN_FORMATS".into(), 7);
        AtomicEnumeration {
            connectors: vec![EnumeratedConnector {
                id: connector_id,
                name: "HDMI-A-1".into(),
                connected: true,
                modes: vec![mode()],
                possible_crtcs: BTreeSet::from([crtc_id]),
                properties: connector_properties,
            }],
            crtcs: vec![EnumeratedCrtc {
                id: crtc_id,
                properties: properties(REQUIRED_CRTC_PROPERTIES),
            }],
            planes: vec![EnumeratedPlane {
                id: plane_id,
                possible_crtcs: BTreeSet::from([crtc_id]),
                properties: plane_properties,
            }],
            blobs: BTreeMap::from([(7, blob)]),
        }
    }

    fn blob(formats: &[u32], modifiers: &[(u64, u32, u64)]) -> Vec<u8> {
        let formats_offset = 24_u32;
        let modifiers_offset = formats_offset + u32::try_from(formats.len() * 4).unwrap();
        let mut blob = Vec::new();
        for value in [
            1_u32,
            0,
            u32::try_from(formats.len()).unwrap(),
            formats_offset,
            u32::try_from(modifiers.len()).unwrap(),
            modifiers_offset,
        ] {
            blob.extend(value.to_ne_bytes());
        }
        for format in formats {
            blob.extend(format.to_ne_bytes());
        }
        for (mask, offset, modifier) in modifiers {
            blob.extend(mask.to_ne_bytes());
            blob.extend(offset.to_ne_bytes());
            blob.extend(0_u32.to_ne_bytes());
            blob.extend(modifier.to_ne_bytes());
        }
        blob
    }

    fn supported(format: ScanoutWgpuFormat) -> ScanoutImportSupport {
        ScanoutImportSupport {
            wgpu_format: Some(format),
            vulkan_external_memory_colour_attachment: true,
            vulkan_external_memory_transfer_src: true,
            mode_extent_supported: true,
            max_extent: Some((u32::MAX, u32::MAX)),
        }
    }

    #[test]
    fn modifier_intersection_and_ranking_prefers_xrgb_and_non_linear_within_a_format() {
        let xrgb_tiled = AtomicFormatModifier {
            fourcc: XRGB,
            modifier: TILED,
        };
        let xrgb_linear = AtomicFormatModifier {
            fourcc: XRGB,
            modifier: LINEAR,
        };
        let xbgr_tiled = AtomicFormatModifier {
            fourcc: XBGR,
            modifier: TILED,
        };
        let enumeration = enumeration(
            10,
            20,
            30,
            blob(&[XRGB, XBGR], &[(0b11, 0, TILED), (0b01, 0, LINEAR)]),
        );
        let mut capabilities = FakeCapabilities {
            vulkan_modifiers: BTreeMap::from([(XRGB, vec![LINEAR, TILED]), (XBGR, vec![TILED])]),
            scanout: BTreeMap::from([
                (xrgb_tiled, supported(ScanoutWgpuFormat::Bgra8Unorm)),
                (xrgb_linear, supported(ScanoutWgpuFormat::Bgra8Unorm)),
                (xbgr_tiled, supported(ScanoutWgpuFormat::Rgba8Unorm)),
            ]),
            gbm: BTreeSet::from([xrgb_tiled, xrgb_linear, xbgr_tiled]),
            ..FakeCapabilities::default()
        };

        let reports = negotiate_atomic_outputs(&enumeration, &mut capabilities);
        assert!(matches!(
            &reports[0].outcome,
            AtomicAdmissionOutcome::Selected(selected)
                if selected.selection.format == XRGB
                    && selected.selection.modifier == TILED
                    && selected.total_candidates == 3
                    && selected.evaluated_candidates == 1
                    && selected.unevaluated_candidates == 2
                    && selected.other_admissible_survivors == AtomicSurvivorStatus::UnknownNotEvaluated
        ));
        assert_eq!(capabilities.gbm_queries, 1);
        assert_eq!(capabilities.last_scanout_size.get(), Some((1920, 1080)));
        assert!(candidate_rank(xrgb_tiled) < candidate_rank(xrgb_linear));
        assert!(candidate_rank(xrgb_linear) < candidate_rank(xbgr_tiled));
    }

    #[test]
    fn missing_in_formats_property_is_a_named_route_rejection() {
        let mut enumeration = enumeration(10, 20, 30, blob(&[XRGB], &[(1, 0, TILED)]));
        enumeration.planes[0].properties.0.remove("IN_FORMATS");
        let mut capabilities = FakeCapabilities::default();

        let reports = negotiate_atomic_outputs(&enumeration, &mut capabilities);
        assert!(matches!(
            &reports[0].outcome,
            AtomicAdmissionOutcome::Rejected(matrix)
                if matrix.route_rejection.as_deref().is_some_and(|reason| reason.contains("IN_FORMATS"))
        ));
    }

    #[test]
    fn empty_intersection_retains_the_full_four_set_rejection_matrix() {
        let enumeration = enumeration(10, 20, 30, blob(&[XRGB], &[(1, 0, TILED)]));
        let candidate = AtomicFormatModifier {
            fourcc: XRGB,
            modifier: TILED,
        };
        let mut capabilities = FakeCapabilities {
            vulkan_modifiers: BTreeMap::from([(XRGB, vec![TILED])]),
            scanout: BTreeMap::from([(
                candidate,
                ScanoutImportSupport {
                    wgpu_format: Some(ScanoutWgpuFormat::Bgra8Unorm),
                    vulkan_external_memory_colour_attachment: false,
                    vulkan_external_memory_transfer_src: false,
                    mode_extent_supported: false,
                    max_extent: None,
                },
            )]),
            gbm: BTreeSet::new(),
            ..FakeCapabilities::default()
        };

        let reports = negotiate_atomic_outputs(&enumeration, &mut capabilities);
        let AtomicAdmissionOutcome::Rejected(matrix) = &reports[0].outcome else {
            panic!("empty intersection must reject");
        };
        assert_eq!(matrix.candidates.len(), 1);
        let row = &matrix.candidates[0];
        assert_eq!(row.plane_in_formats, AtomicCapabilityState::Supported);
        assert!(matches!(
            row.gbm_allocation,
            AtomicCapabilityState::Rejected(_)
        ));
        assert!(matches!(
            row.vulkan_colour_attachment,
            AtomicCapabilityState::Rejected(_)
        ));
        assert_eq!(row.wgpu_render_attachment, AtomicCapabilityState::Supported);
        assert_eq!(row.selection_policy, AtomicCapabilityState::Supported);
        assert_eq!(capabilities.gbm_queries, 1);
    }

    #[test]
    fn transfer_src_and_mode_extent_fail_as_named_vulkan_matrix_rejections() {
        let candidate = AtomicFormatModifier {
            fourcc: XRGB,
            modifier: TILED,
        };
        let plane_formats = BTreeSet::from([candidate]);
        let mut transfer_rejected = FakeCapabilities {
            scanout: BTreeMap::from([(
                candidate,
                ScanoutImportSupport {
                    vulkan_external_memory_transfer_src: false,
                    mode_extent_supported: false,
                    max_extent: None,
                    ..supported(ScanoutWgpuFormat::Bgra8Unorm)
                },
            )]),
            gbm: BTreeSet::from([candidate]),
            ..FakeCapabilities::default()
        };
        let transfer_row =
            evaluate_candidate(&mut transfer_rejected, &plane_formats, mode(), candidate);
        assert!(matches!(
            transfer_row.vulkan_colour_attachment,
            AtomicCapabilityState::Rejected(reason) if reason.contains("TRANSFER_SRC")
        ));

        let mut extent_rejected = FakeCapabilities {
            scanout: BTreeMap::from([(
                candidate,
                ScanoutImportSupport {
                    mode_extent_supported: false,
                    max_extent: Some((1280, 720)),
                    ..supported(ScanoutWgpuFormat::Bgra8Unorm)
                },
            )]),
            gbm: BTreeSet::from([candidate]),
            ..FakeCapabilities::default()
        };
        let extent_row =
            evaluate_candidate(&mut extent_rejected, &plane_formats, mode(), candidate);
        assert!(matches!(
            extent_row.vulkan_colour_attachment,
            AtomicCapabilityState::Rejected(reason)
                if reason.contains("1920x1080") && reason.contains("1280x720")
        ));
        assert_eq!(extent_rejected.last_scanout_size.get(), Some((1920, 1080)));
    }

    #[test]
    fn alpha_formats_remain_in_the_matrix_but_cannot_be_selected() {
        let candidate = AtomicFormatModifier {
            fourcc: ARGB,
            modifier: TILED,
        };
        let enumeration = enumeration(10, 20, 30, blob(&[ARGB], &[(1, 0, TILED)]));
        let mut capabilities = FakeCapabilities {
            scanout: BTreeMap::from([(candidate, supported(ScanoutWgpuFormat::Bgra8Unorm))]),
            gbm: BTreeSet::from([candidate]),
            ..FakeCapabilities::default()
        };

        let reports = negotiate_atomic_outputs(&enumeration, &mut capabilities);
        let AtomicAdmissionOutcome::Rejected(matrix) = &reports[0].outcome else {
            panic!("alpha-only plane must reject");
        };
        assert_eq!(matrix.candidates.len(), 1);
        assert!(matches!(
            &matrix.candidates[0].selection_policy,
            AtomicCapabilityState::Rejected(reason) if reason.contains("opaque")
        ));
    }

    #[test]
    fn recycled_connector_crtc_and_plane_ids_replace_the_previous_route() {
        let candidate = AtomicFormatModifier {
            fourcc: XRGB,
            modifier: TILED,
        };
        let make_capabilities = || FakeCapabilities {
            vulkan_modifiers: BTreeMap::from([(XRGB, vec![TILED])]),
            scanout: BTreeMap::from([(candidate, supported(ScanoutWgpuFormat::Bgra8Unorm))]),
            gbm: BTreeSet::from([candidate]),
            ..FakeCapabilities::default()
        };
        let first = enumeration(10, 20, 30, blob(&[XRGB], &[(1, 0, TILED)]));
        let second = enumeration(11, 21, 31, blob(&[XRGB], &[(1, 0, TILED)]));
        let mut first_caps = make_capabilities();
        let mut second_caps = make_capabilities();

        let first = negotiate_atomic_outputs(&first, &mut first_caps);
        let second = negotiate_atomic_outputs(&second, &mut second_caps);
        assert!(
            matches!(&first[0].outcome, AtomicAdmissionOutcome::Selected(selected) if (selected.selection.connector_id, selected.selection.crtc_id, selected.selection.primary_plane_id) == (10, 20, 30))
        );
        assert!(
            matches!(&second[0].outcome, AtomicAdmissionOutcome::Selected(selected) if (selected.selection.connector_id, selected.selection.crtc_id, selected.selection.primary_plane_id) == (11, 21, 31))
        );
    }

    #[test]
    fn ambiguous_and_incompatible_routes_are_rejected_before_format_queries() {
        let mut ambiguous = enumeration(10, 20, 30, blob(&[XRGB], &[(1, 0, TILED)]));
        ambiguous.connectors[0]
            .properties
            .0
            .insert("CRTC_ID".into(), 0);
        ambiguous.connectors[0].possible_crtcs.insert(21);
        ambiguous.crtcs.push(EnumeratedCrtc {
            id: 21,
            properties: properties(REQUIRED_CRTC_PROPERTIES),
        });
        let mut incompatible = ambiguous.clone();
        incompatible.connectors[0].possible_crtcs.clear();
        let mut ambiguous_planes = enumeration(10, 20, 30, blob(&[XRGB], &[(1, 0, TILED)]));
        let mut second_plane = ambiguous_planes.planes[0].clone();
        second_plane.id = 31;
        ambiguous_planes.planes.push(second_plane);
        let mut capabilities = FakeCapabilities::default();

        let ambiguous = negotiate_atomic_outputs(&ambiguous, &mut capabilities);
        let incompatible = negotiate_atomic_outputs(&incompatible, &mut capabilities);
        let ambiguous_planes = negotiate_atomic_outputs(&ambiguous_planes, &mut capabilities);
        assert!(
            matches!(&ambiguous[0].outcome, AtomicAdmissionOutcome::Rejected(matrix) if matrix.route_rejection.as_deref().is_some_and(|reason| reason.contains("ambiguous")))
        );
        assert!(
            matches!(&incompatible[0].outcome, AtomicAdmissionOutcome::Rejected(matrix) if matrix.route_rejection.as_deref().is_some_and(|reason| reason.contains("no compatible CRTC")))
        );
        assert!(
            matches!(&ambiguous_planes[0].outcome, AtomicAdmissionOutcome::Rejected(matrix) if matrix.route_rejection.as_deref().is_some_and(|reason| reason.contains("ambiguous primary planes")))
        );
    }

    #[test]
    fn malformed_in_formats_blob_is_bounded_and_rejected() {
        let error = decode_in_formats(&[0; 12]).expect_err("short blob rejects");
        assert!(error.contains("shorter"));
    }

    fn overwrite_u32(blob: &mut [u8], offset: usize, value: u32) {
        blob[offset..offset + 4].copy_from_slice(&value.to_ne_bytes());
    }

    #[test]
    fn in_formats_rejects_an_unknown_version() {
        let mut value = blob(&[XRGB], &[(1, 0, TILED)]);
        overwrite_u32(&mut value, 0, 2);
        assert!(decode_in_formats(&value).unwrap_err().contains("version 2"));
    }

    #[test]
    fn in_formats_rejects_a_table_offset_past_the_payload() {
        let mut value = blob(&[XRGB], &[(1, 0, TILED)]);
        let past_end = u32::try_from(value.len() + 1).unwrap();
        overwrite_u32(&mut value, 12, past_end);
        assert!(
            decode_in_formats(&value)
                .unwrap_err()
                .contains("past the payload")
        );
    }

    #[test]
    fn in_formats_rejects_a_table_start_inside_the_header() {
        let mut value = blob(&[XRGB], &[(1, 0, TILED)]);
        overwrite_u32(&mut value, 12, 20);
        assert!(decode_in_formats(&value).unwrap_err().contains("header"));
    }

    #[test]
    fn in_formats_rejects_oversized_counts() {
        let mut value = blob(&[XRGB], &[(1, 0, TILED)]);
        overwrite_u32(&mut value, 8, u32::MAX);
        assert!(
            decode_in_formats(&value)
                .unwrap_err()
                .contains("past the payload")
        );
    }

    #[test]
    fn in_formats_rejects_overlapping_tables() {
        let mut value = blob(&[XRGB], &[(1, 0, TILED)]);
        overwrite_u32(&mut value, 20, 24);
        assert!(decode_in_formats(&value).unwrap_err().contains("overlap"));
    }

    #[test]
    fn in_formats_rejects_a_mask_index_beyond_the_format_table() {
        let value = blob(&[XRGB], &[(0b10, 0, TILED)]);
        assert!(
            decode_in_formats(&value)
                .unwrap_err()
                .contains("format index 1")
        );
    }

    #[test]
    fn in_formats_format_index_addition_is_checked() {
        assert!(
            checked_format_index(usize::MAX, 1)
                .unwrap_err()
                .contains("overflow")
        );
    }

    #[test]
    fn opened_drm_device_identity_mismatch_is_named() {
        assert!(validate_opened_device(10, 10).is_ok());
        let error = validate_opened_device(10, 11).unwrap_err();
        assert!(error.contains("expected dev_t 10"));
        assert!(error.contains("got 11"));
    }
}
