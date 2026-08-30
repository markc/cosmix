//! Read-only DRM connector discovery and pure scan differencing.
//!
//! Connector enumeration comes from sysfs, avoiding drm-ffi's unsound
//! two-ioctl resource-vector growth path. A primary-node open can implicitly
//! acquire DRM master even when it is read-only. The watcher retains that
//! kernel grant for the lifetime of the card, but never tries to acquire
//! master. Connector detection is forced only while the retained fd is the
//! current master; other cards use cached metadata.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    fs::{self, File, OpenOptions},
    io,
    num::NonZeroU32,
    os::{
        fd::{AsFd, BorrowedFd},
        unix::fs::MetadataExt,
    },
    path::{Path, PathBuf},
};

#[cfg(any(feature = "kms-live", test))]
use std::os::fd::AsRawFd;

use smithay::reexports::{
    drm::{
        self,
        control::{
            Device as ControlDevice, Mode, ModeFlags, ModeTypeFlags,
            connector::{self, State as DrmConnectorState},
        },
    },
    rustix::{io::Errno, ioctl},
};

use super::kms::{ConnectorDescription, ConnectorMode, DeviceId, OutputKey};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ConnectorProbe {
    Cached,
    Forced,
}

impl ConnectorProbe {
    fn force_probe(self) -> bool {
        self == Self::Forced
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Cached => "cached-kernel-metadata-not-drm-master",
            Self::Forced => "forced-driver-detection-as-drm-master",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DrmMasterState {
    RetainedImplicit,
    NotMaster,
}

impl DrmMasterState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::RetainedImplicit => "retained-implicit-master",
            Self::NotMaster => "not-master-existing-owner-or-ineligible",
        }
    }

    fn connector_probe(self, requested: ConnectorProbe) -> ConnectorProbe {
        match (self, requested) {
            (Self::RetainedImplicit, ConnectorProbe::Forced) => ConnectorProbe::Forced,
            (Self::RetainedImplicit | Self::NotMaster, ConnectorProbe::Cached)
            | (Self::NotMaster, ConnectorProbe::Forced) => ConnectorProbe::Cached,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CardCloseOutcome {
    ClosedRetainedImplicitMaster,
    ClosedNonMaster,
}

impl CardCloseOutcome {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::ClosedRetainedImplicitMaster => "closed-fd-released-retained-implicit-master",
            Self::ClosedNonMaster => "closed-non-master-fd",
        }
    }
}

impl From<DrmMasterState> for CardCloseOutcome {
    fn from(state: DrmMasterState) -> Self {
        match state {
            DrmMasterState::RetainedImplicit => Self::ClosedRetainedImplicitMaster,
            DrmMasterState::NotMaster => Self::ClosedNonMaster,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ConnectorStatus {
    Connected,
    Disconnected,
    Unknown,
}

impl ConnectorStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Connected => "connected",
            Self::Disconnected => "disconnected",
            Self::Unknown => "unknown",
        }
    }
}

impl From<DrmConnectorState> for ConnectorStatus {
    fn from(status: DrmConnectorState) -> Self {
        match status {
            DrmConnectorState::Connected => Self::Connected,
            DrmConnectorState::Disconnected => Self::Disconnected,
            DrmConnectorState::Unknown => Self::Unknown,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ConnectorInfo {
    pub(crate) key: OutputKey,
    pub(crate) connector_id: u32,
    pub(crate) name: String,
    pub(crate) interface: String,
    pub(crate) interface_id: u32,
    pub(crate) status: ConnectorStatus,
    pub(crate) physical_size_mm: Option<(u32, u32)>,
    pub(crate) modes: Vec<ConnectorMode>,
}

impl ConnectorInfo {
    pub(crate) fn description(&self) -> Option<ConnectorDescription> {
        (self.status == ConnectorStatus::Connected).then(|| ConnectorDescription {
            key: self.key.clone(),
            connector_id: self.connector_id,
            modes: self.modes.clone(),
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ConnectorScan {
    pub(crate) device: DeviceId,
    pub(crate) path: PathBuf,
    connector_probe: Option<ConnectorProbe>,
    drm_master_state: Option<DrmMasterState>,
    connectors: BTreeMap<String, ConnectorInfo>,
}

impl ConnectorScan {
    pub(crate) fn empty(device: DeviceId, path: PathBuf) -> Self {
        Self {
            device,
            path,
            connector_probe: None,
            drm_master_state: None,
            connectors: BTreeMap::new(),
        }
    }

    pub(crate) fn from_connectors(
        device: DeviceId,
        path: PathBuf,
        connector_probe: ConnectorProbe,
        drm_master_state: DrmMasterState,
        connectors: impl IntoIterator<Item = ConnectorInfo>,
    ) -> Self {
        Self {
            device,
            path,
            connector_probe: Some(connector_probe),
            drm_master_state: Some(drm_master_state),
            connectors: connectors
                .into_iter()
                .map(|connector| (connector.key.connector_name.clone(), connector))
                .collect(),
        }
    }

    pub(crate) fn from_scanned_connectors(
        device: DeviceId,
        path: PathBuf,
        connector_probe: ConnectorProbe,
        drm_master_state: DrmMasterState,
        connectors: impl IntoIterator<Item = ConnectorInfo>,
    ) -> Self {
        Self::from_connectors(device, path, connector_probe, drm_master_state, connectors)
    }

    pub(crate) fn connector_probe(&self) -> Option<ConnectorProbe> {
        self.connector_probe
    }

    pub(crate) fn drm_master_state(&self) -> Option<DrmMasterState> {
        self.drm_master_state
    }

    pub(crate) fn connectors(&self) -> impl Iterator<Item = &ConnectorInfo> {
        self.connectors.values()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ConnectorDiff {
    Added {
        connector: ConnectorInfo,
    },
    Changed {
        previous: ConnectorInfo,
        connector: ConnectorInfo,
    },
    Removed {
        connector: ConnectorInfo,
    },
}

impl ConnectorDiff {
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Self::Added { .. } => "add",
            Self::Changed { .. } => "change",
            Self::Removed { .. } => "remove",
        }
    }

    pub(crate) fn connector(&self) -> &ConnectorInfo {
        match self {
            Self::Added { connector }
            | Self::Changed { connector, .. }
            | Self::Removed { connector } => connector,
        }
    }
}

/// Compare two complete snapshots of one card.
///
/// The caller enforces that both snapshots belong to the same card before
/// entering this pure comparison.
pub(crate) fn diff_connector_scans(
    previous: &ConnectorScan,
    next: &ConnectorScan,
) -> Vec<ConnectorDiff> {
    let connector_names = previous
        .connectors
        .keys()
        .chain(next.connectors.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut diffs = Vec::new();

    for connector_name in connector_names {
        match (
            previous.connectors.get(&connector_name),
            next.connectors.get(&connector_name),
        ) {
            (None, Some(connector)) => diffs.push(ConnectorDiff::Added {
                connector: connector.clone(),
            }),
            (Some(previous), Some(connector)) if previous != connector => {
                diffs.push(ConnectorDiff::Changed {
                    previous: previous.clone(),
                    connector: connector.clone(),
                });
            }
            (Some(connector), None) => diffs.push(ConnectorDiff::Removed {
                connector: connector.clone(),
            }),
            (Some(_), Some(_)) | (None, None) => {}
        }
    }

    diffs
}

#[derive(Debug)]
pub(crate) enum ConnectorScanError {
    Open {
        path: PathBuf,
        source: std::io::Error,
    },
    Metadata {
        path: PathBuf,
        source: std::io::Error,
    },
    DeviceIdentity {
        path: PathBuf,
        expected: DeviceId,
        actual: DeviceId,
    },
    WatchedPath {
        device: DeviceId,
        expected: PathBuf,
        actual: PathBuf,
    },
    CardName {
        path: PathBuf,
    },
    SysfsEnumerate {
        path: PathBuf,
        source: io::Error,
    },
    SysfsConnectorId {
        path: PathBuf,
        source: String,
    },
    MasterCheck {
        path: PathBuf,
        source: io::Error,
    },
    Connector {
        path: PathBuf,
        connector_id: u32,
        source: std::io::Error,
    },
    ConnectorIdentity {
        path: PathBuf,
        connector_id: u32,
        expected_name: String,
        actual_name: String,
    },
}

impl fmt::Display for ConnectorScanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Open { path, source } => {
                write!(
                    formatter,
                    "could not open DRM card {} read-only: {source}",
                    path.display()
                )
            }
            Self::Metadata { path, source } => {
                write!(
                    formatter,
                    "could not inspect DRM card {}: {source}",
                    path.display()
                )
            }
            Self::DeviceIdentity {
                path,
                expected,
                actual,
            } => write!(
                formatter,
                "DRM card {} has dev_t {actual}, expected {expected}",
                path.display()
            ),
            Self::WatchedPath {
                device,
                expected,
                actual,
            } => write!(
                formatter,
                "DRM dev_t {device} is already watched as {}, not {}",
                expected.display(),
                actual.display()
            ),
            Self::CardName { path } => write!(
                formatter,
                "DRM card path {} has no canonical cardN filename",
                path.display()
            ),
            Self::SysfsEnumerate { path, source } => write!(
                formatter,
                "could not enumerate DRM connectors below {}: {source}",
                path.display()
            ),
            Self::SysfsConnectorId { path, source } => write!(
                formatter,
                "could not read connector identity from {}: {source}",
                path.display()
            ),
            Self::MasterCheck { path, source } => write!(
                formatter,
                "could not determine whether DRM card {} granted implicit master: {source}",
                path.display()
            ),
            Self::Connector {
                path,
                connector_id,
                source,
            } => write!(
                formatter,
                "drmModeGetConnector failed for {} connector {connector_id}: {source}",
                path.display()
            ),
            Self::ConnectorIdentity {
                path,
                connector_id,
                expected_name,
                actual_name,
            } => write!(
                formatter,
                "DRM card {} connector {connector_id} resolved as {actual_name}, expected sysfs \
                 identity {expected_name}",
                path.display()
            ),
        }
    }
}

impl std::error::Error for ConnectorScanError {}

#[derive(Debug)]
struct ReadOnlyCard(File);

impl AsFd for ReadOnlyCard {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl drm::Device for ReadOnlyCard {}
impl ControlDevice for ReadOnlyCard {}

#[cfg(any(feature = "kms-live", test))]
#[derive(Debug)]
struct BorrowedCard<'fd>(BorrowedFd<'fd>);

#[cfg(any(feature = "kms-live", test))]
impl AsFd for BorrowedCard<'_> {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0
    }
}

#[cfg(any(feature = "kms-live", test))]
impl drm::Device for BorrowedCard<'_> {}
#[cfg(any(feature = "kms-live", test))]
impl ControlDevice for BorrowedCard<'_> {}

#[derive(Default)]
pub(crate) struct ConnectorScanner {
    cards: BTreeMap<DeviceId, WatchedCard>,
}

#[derive(Debug)]
struct WatchedCard {
    path: PathBuf,
    card: ReadOnlyCard,
    drm_master_state: DrmMasterState,
}

impl ConnectorScanner {
    pub(crate) fn scan(
        &mut self,
        device: DeviceId,
        path: &Path,
        requested_probe: ConnectorProbe,
    ) -> Result<ConnectorScan, ConnectorScanError> {
        if let std::collections::btree_map::Entry::Vacant(entry) = self.cards.entry(device) {
            let watched = open_card_with(device, path, |path| {
                OpenOptions::new().read(true).open(path)
            })?;
            entry.insert(watched);
        }
        let watched = self
            .cards
            .get(&device)
            .expect("card was inserted or already watched");
        if watched.path != path {
            return Err(ConnectorScanError::WatchedPath {
                device,
                expected: watched.path.clone(),
                actual: path.to_path_buf(),
            });
        }
        scan_card(
            device,
            path,
            Path::new("/sys/class/drm"),
            &watched.card,
            watched.drm_master_state,
            requested_probe,
        )
    }

    pub(crate) fn stop_watching(&mut self, device: DeviceId) -> Option<CardCloseOutcome> {
        self.cards
            .remove(&device)
            .map(|watched| watched.drm_master_state.into())
    }

    pub(crate) fn stop_all(&mut self) -> BTreeMap<DeviceId, CardCloseOutcome> {
        std::mem::take(&mut self.cards)
            .into_iter()
            .map(|(device, watched)| (device, watched.drm_master_state.into()))
            .collect()
    }
}

/// Scan through a DRM fd owned by the caller.
///
/// The opener is a negative-capability test seam and is deliberately discarded:
/// the path only validates identity and locates sysfs; every DRM ioctl uses `fd`.
#[cfg(any(feature = "kms-live", test))]
pub(crate) fn scan_borrowed_card<O, F>(
    device: DeviceId,
    path: &Path,
    fd: BorrowedFd<'_>,
    sysfs_drm: &Path,
    requested_probe: ConnectorProbe,
    open_card: O,
    observe_master: F,
) -> Result<ConnectorScan, ConnectorScanError>
where
    O: FnOnce(&Path) -> io::Result<File>,
    F: FnOnce(BorrowedFd<'_>) -> io::Result<DrmMasterState>,
{
    drop(open_card);
    validate_path_device(device, path)?;
    let actual = borrowed_fd_device(fd).map_err(|source| ConnectorScanError::Metadata {
        path: path.to_path_buf(),
        source,
    })?;
    if actual != device {
        return Err(ConnectorScanError::DeviceIdentity {
            path: path.to_path_buf(),
            expected: device,
            actual,
        });
    }

    let drm_master_state =
        observe_master(fd).map_err(|source| ConnectorScanError::MasterCheck {
            path: path.to_path_buf(),
            source,
        })?;
    let card = BorrowedCard(fd);
    scan_card(
        device,
        path,
        sysfs_drm,
        &card,
        drm_master_state,
        requested_probe,
    )
}

#[cfg(all(feature = "kms-live", not(test)))]
pub(crate) fn borrowed_master_state(fd: BorrowedFd<'_>) -> io::Result<DrmMasterState> {
    current_master_state(&BorrowedCard(fd))
}

fn open_card_with<F>(
    device: DeviceId,
    path: &Path,
    open: F,
) -> Result<WatchedCard, ConnectorScanError>
where
    F: FnOnce(&Path) -> std::io::Result<File>,
{
    validate_path_device(device, path)?;
    let file = open(path).map_err(|source| ConnectorScanError::Open {
        path: path.to_path_buf(),
        source,
    })?;
    let actual = file
        .metadata()
        .map_err(|source| ConnectorScanError::Metadata {
            path: path.to_path_buf(),
            source,
        })?
        .rdev();
    if actual != device {
        return Err(ConnectorScanError::DeviceIdentity {
            path: path.to_path_buf(),
            expected: device,
            actual,
        });
    }

    let card = ReadOnlyCard(file);
    let drm_master_state =
        current_master_state(&card).map_err(|source| ConnectorScanError::MasterCheck {
            path: path.to_path_buf(),
            source,
        })?;
    Ok(WatchedCard {
        path: path.to_path_buf(),
        card,
        drm_master_state,
    })
}

fn validate_path_device(device: DeviceId, path: &Path) -> Result<(), ConnectorScanError> {
    let actual = fs::metadata(path)
        .map_err(|source| ConnectorScanError::Metadata {
            path: path.to_path_buf(),
            source,
        })?
        .rdev();
    if actual != device {
        return Err(ConnectorScanError::DeviceIdentity {
            path: path.to_path_buf(),
            expected: device,
            actual,
        });
    }
    Ok(())
}

#[cfg(any(feature = "kms-live", test))]
fn borrowed_fd_device(fd: BorrowedFd<'_>) -> io::Result<DeviceId> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: `stat` is valid writable storage and is read only after fstat
    // reports that it initialised the value from the still-borrowed fd.
    if unsafe { libc::fstat(fd.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { stat.assume_init() }.st_rdev)
}

fn scan_card<C>(
    device: DeviceId,
    path: &Path,
    sysfs_drm: &Path,
    card: &C,
    drm_master_state: DrmMasterState,
    requested_probe: ConnectorProbe,
) -> Result<ConnectorScan, ConnectorScanError>
where
    C: ControlDevice,
{
    let connector_handles = sysfs_connector_handles(path, sysfs_drm)?;
    let connector_probe = drm_master_state.connector_probe(requested_probe);
    let mut connectors = Vec::with_capacity(connector_handles.len());
    for (expected_name, connector_id) in connector_handles {
        let handle = connector::Handle::from(
            NonZeroU32::new(connector_id).expect("sysfs parser rejects zero connector ids"),
        );
        let info = card
            .get_connector(handle, connector_probe.force_probe())
            .map_err(|source| ConnectorScanError::Connector {
                path: path.to_path_buf(),
                connector_id,
                source,
            })?;
        let actual_name = info.to_string();
        if !connector_identity_matches(&expected_name, &actual_name) {
            return Err(ConnectorScanError::ConnectorIdentity {
                path: path.to_path_buf(),
                connector_id,
                expected_name,
                actual_name,
            });
        }
        connectors.push(connector_info(device, connector_id, &info));
    }

    Ok(ConnectorScan::from_scanned_connectors(
        device,
        path.to_path_buf(),
        connector_probe,
        drm_master_state,
        connectors,
    ))
}

fn sysfs_connector_handles(
    card_path: &Path,
    sysfs_drm: &Path,
) -> Result<Vec<(String, u32)>, ConnectorScanError> {
    let card_name = card_path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| is_primary_card_name(name))
        .ok_or_else(|| ConnectorScanError::CardName {
            path: card_path.to_path_buf(),
        })?;
    let prefix = format!("{card_name}-");
    let entries = fs::read_dir(sysfs_drm).map_err(|source| ConnectorScanError::SysfsEnumerate {
        path: sysfs_drm.to_path_buf(),
        source,
    })?;
    let mut handles = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| ConnectorScanError::SysfsEnumerate {
            path: sysfs_drm.to_path_buf(),
            source,
        })?;
        let entry_name = entry.file_name();
        let Some(entry_name) = entry_name.to_str() else {
            continue;
        };
        let Some(connector_name) = entry_name.strip_prefix(&prefix) else {
            continue;
        };
        if connector_name.is_empty() {
            continue;
        }
        let identity_path = entry.path().join("connector_id");
        let raw_id = fs::read_to_string(&identity_path).map_err(|source| {
            ConnectorScanError::SysfsConnectorId {
                path: identity_path.clone(),
                source: source.to_string(),
            }
        })?;
        let connector_id = raw_id.trim().parse::<u32>().map_err(|source| {
            ConnectorScanError::SysfsConnectorId {
                path: identity_path.clone(),
                source: source.to_string(),
            }
        })?;
        if connector_id == 0 {
            return Err(ConnectorScanError::SysfsConnectorId {
                path: identity_path,
                source: "connector id is zero".into(),
            });
        }
        handles.push((connector_name.to_owned(), connector_id));
    }
    handles.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(handles)
}

fn is_primary_card_name(name: &str) -> bool {
    name.strip_prefix("card").is_some_and(|suffix| {
        !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
    })
}

#[repr(C)]
struct DrmAuth {
    magic: u32,
}

const DRM_IOCTL_AUTH_MAGIC: ioctl::Opcode = ioctl::opcode::write::<DrmAuth>(b'd', 0x11);

fn current_master_state(card: &impl drm::Device) -> io::Result<DrmMasterState> {
    // This is the same authority check as libdrm's drmIsMaster: attempting to
    // authenticate invalid magic 0 is rejected with EACCES before lookup for a
    // non-master, and reaches lookup (EINVAL) for the current master. It neither
    // acquires nor releases master.
    let check =
        unsafe { ioctl::Setter::<DRM_IOCTL_AUTH_MAGIC, DrmAuth>::new(DrmAuth { magic: 0 }) };
    let result = unsafe { ioctl::ioctl(card, check) };
    accept_master_check_result(result)
}

fn accept_master_check_result(result: Result<(), Errno>) -> io::Result<DrmMasterState> {
    match result {
        Ok(()) => Ok(DrmMasterState::RetainedImplicit),
        Err(Errno::ACCESS) => Ok(DrmMasterState::NotMaster),
        Err(Errno::INVAL) => Ok(DrmMasterState::RetainedImplicit),
        Err(error) => Err(error.into()),
    }
}

fn connector_identity_matches(expected_name: &str, actual_name: &str) -> bool {
    expected_name == actual_name
}

fn connector_info(device: DeviceId, connector_id: u32, info: &connector::Info) -> ConnectorInfo {
    let name = info.to_string();
    let interface = info.interface().as_str().to_owned();
    ConnectorInfo {
        key: OutputKey {
            device,
            connector_name: name.clone(),
        },
        connector_id,
        name,
        interface,
        interface_id: info.interface_id(),
        status: info.state().into(),
        physical_size_mm: info.size(),
        modes: info.modes().iter().map(connector_mode).collect(),
    }
}

pub(super) fn connector_mode(mode: &Mode) -> ConnectorMode {
    let (width, height) = mode.size();
    ConnectorMode {
        width: u32::from(width),
        height: u32::from(height),
        refresh_millihz: exact_refresh_millihz(mode),
        preferred: mode.mode_type().contains(ModeTypeFlags::PREFERRED),
        clock_khz: mode.clock(),
        hsync: mode.hsync(),
        vsync: mode.vsync(),
        hskew: mode.hskew(),
        vscan: mode.vscan(),
        flags: mode.flags().bits(),
    }
}

fn exact_refresh_millihz(mode: &Mode) -> u32 {
    let (_, _, htotal) = mode.hsync();
    let (_, _, vtotal) = mode.vsync();
    exact_refresh_from_timing(
        mode.clock(),
        htotal,
        vtotal,
        mode.vrefresh(),
        mode.vscan(),
        mode.flags().contains(ModeFlags::INTERLACE),
        mode.flags().contains(ModeFlags::DBLSCAN),
    )
}

pub(super) fn exact_refresh_from_timing(
    clock_khz: u32,
    htotal: u16,
    vtotal: u16,
    fallback_vrefresh_hz: u32,
    vscan: u16,
    interlaced: bool,
    doublescan: bool,
) -> u32 {
    if htotal == 0 || vtotal == 0 {
        return fallback_vrefresh_hz.saturating_mul(1_000);
    }

    let mut numerator = u64::from(clock_khz) * 1_000_000;
    let mut denominator = u64::from(htotal) * u64::from(vtotal);
    if interlaced {
        numerator = numerator.saturating_mul(2);
    }
    if doublescan {
        denominator = denominator.saturating_mul(2);
    }
    if vscan > 1 {
        denominator = denominator.saturating_mul(u64::from(vscan));
    }
    let rounded = numerator.saturating_add(denominator / 2) / denominator;
    u32::try_from(rounded).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use std::{
        cell::Cell,
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::*;

    static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(1);

    type ConnectorMutation = Box<dyn FnOnce(&mut ConnectorInfo)>;

    fn connector(device: DeviceId, id: u32) -> ConnectorInfo {
        ConnectorInfo {
            key: OutputKey {
                device,
                connector_name: "Virtual-1".into(),
            },
            connector_id: id,
            name: "Virtual-1".into(),
            interface: "Virtual".into(),
            interface_id: 1,
            status: ConnectorStatus::Connected,
            physical_size_mm: Some((600, 340)),
            modes: vec![ConnectorMode {
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
            }],
        }
    }

    fn scan(connectors: impl IntoIterator<Item = ConnectorInfo>) -> ConnectorScan {
        ConnectorScan::from_connectors(
            200,
            "/dev/dri/card1".into(),
            ConnectorProbe::Cached,
            DrmMasterState::NotMaster,
            connectors,
        )
    }

    #[test]
    fn borrowed_scanner_uses_the_supplied_fd_without_a_card_opener() {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "cosmix-borrowed-scan-{}-{sequence}",
            std::process::id()
        ));
        let sysfs = root.join("sysfs");
        fs::create_dir(&root).expect("create unique borrowed-scan directory");
        fs::create_dir(&sysfs).expect("create fake sysfs directory");
        let path = root.join("card123");
        let file = File::create(&path).expect("create non-DRM borrowed-fd fixture");
        let expected_fd = file.as_raw_fd();
        let opens = Cell::new(0_u32);

        let scanned = scan_borrowed_card(
            0,
            &path,
            file.as_fd(),
            &sysfs,
            ConnectorProbe::Forced,
            |path| {
                opens.set(opens.get() + 1);
                OpenOptions::new().read(true).open(path)
            },
            |fd| {
                assert_eq!(fd.as_raw_fd(), expected_fd);
                Ok(DrmMasterState::NotMaster)
            },
        )
        .expect("empty fake sysfs scans through the borrowed fd");

        assert_eq!(
            opens.get(),
            0,
            "borrowed scanner must never invoke an opener"
        );
        assert_eq!(scanned.device, 0);
        assert_eq!(scanned.path, path);
        assert_eq!(scanned.connector_probe(), Some(ConnectorProbe::Cached));
        assert_eq!(scanned.drm_master_state(), Some(DrmMasterState::NotMaster));
        assert_eq!(scanned.connectors().count(), 0);
        assert_eq!(
            file.metadata().expect("borrowed fd remains owned").rdev(),
            0
        );

        drop(file);
        fs::remove_file(&path).expect("remove borrowed-fd fixture");
        fs::remove_dir(&sysfs).expect("remove fake sysfs directory");
        fs::remove_dir(&root).expect("remove unique borrowed-scan directory");
    }

    #[test]
    fn connector_probe_policy_maps_only_forced_detection_to_the_force_ioctl_flag() {
        assert!(!ConnectorProbe::Cached.force_probe());
        assert!(ConnectorProbe::Forced.force_probe());
    }

    #[test]
    fn forced_detection_is_downgraded_only_when_the_watched_fd_is_not_master() {
        assert_eq!(
            DrmMasterState::RetainedImplicit.connector_probe(ConnectorProbe::Forced),
            ConnectorProbe::Forced
        );
        assert_eq!(
            DrmMasterState::NotMaster.connector_probe(ConnectorProbe::Forced),
            ConnectorProbe::Cached
        );
    }

    #[test]
    fn connected_description_preserves_numeric_id_without_changing_stable_key() {
        let connector = connector(200, 31);
        let description = connector.description().expect("connected description");

        assert_eq!(description.key.connector_name, "Virtual-1");
        assert_eq!(description.connector_id, 31);
    }

    #[test]
    fn multi_vscan_uses_the_same_timing_rational_as_mesa_display_wsi() {
        assert_eq!(
            exact_refresh_from_timing(148_500, 2_200, 1_125, 60, 2, false, false),
            30_000
        );
    }

    #[test]
    fn zero_total_uses_the_drm_vrefresh_fallback() {
        assert_eq!(
            exact_refresh_from_timing(148_500, 0, 1_125, 60, 2, false, false),
            60_000
        );
    }

    #[test]
    fn appeared_connector_is_the_only_difference() {
        let connector = connector(200, 31);
        assert_eq!(
            diff_connector_scans(&scan([]), &scan([connector.clone()])),
            vec![ConnectorDiff::Added { connector }]
        );
    }

    #[test]
    fn changed_connector_detects_each_observable_property_in_isolation() {
        let original = connector(200, 31);
        let mutations: Vec<ConnectorMutation> = vec![
            Box::new(|connector| connector.status = ConnectorStatus::Disconnected),
            Box::new(|connector| connector.name = "Virtual-2".into()),
            Box::new(|connector| connector.connector_id = 32),
            Box::new(|connector| connector.interface = "DisplayPort".into()),
            Box::new(|connector| connector.interface_id = 2),
            Box::new(|connector| connector.physical_size_mm = Some((700, 400))),
            Box::new(|connector| connector.modes[0].width += 1),
            Box::new(|connector| connector.modes[0].height += 1),
            Box::new(|connector| connector.modes[0].refresh_millihz += 1),
            Box::new(|connector| connector.modes[0].preferred = false),
            Box::new(|connector| connector.modes[0].clock_khz += 1),
            Box::new(|connector| connector.modes[0].hsync.2 += 1),
            Box::new(|connector| connector.modes[0].vsync.2 += 1),
            Box::new(|connector| connector.modes[0].hskew += 1),
            Box::new(|connector| connector.modes[0].vscan += 1),
            Box::new(|connector| connector.modes[0].flags = 1),
        ];

        for mutate in mutations {
            let mut changed = original.clone();
            mutate(&mut changed);
            assert_eq!(
                diff_connector_scans(&scan([original.clone()]), &scan([changed.clone()])),
                vec![ConnectorDiff::Changed {
                    previous: original.clone(),
                    connector: changed,
                }]
            );
        }
    }

    #[test]
    fn gone_connector_is_the_only_difference() {
        let connector = connector(200, 31);
        assert_eq!(
            diff_connector_scans(&scan([connector.clone()]), &scan([])),
            vec![ConnectorDiff::Removed { connector }]
        );
    }

    #[test]
    fn identical_scans_have_no_difference() {
        let connector = connector(200, 31);
        assert!(diff_connector_scans(&scan([connector.clone()]), &scan([connector])).is_empty());
    }

    #[test]
    fn recycled_raw_connector_id_with_a_new_sysfs_name_is_remove_plus_add() {
        let original = connector(200, 31);
        let replacement = ConnectorInfo {
            key: OutputKey {
                device: 200,
                connector_name: "Virtual-2".into(),
            },
            name: "Virtual-2".into(),
            ..original.clone()
        };

        assert_eq!(
            diff_connector_scans(&scan([original.clone()]), &scan([replacement.clone()])),
            vec![
                ConnectorDiff::Removed {
                    connector: original,
                },
                ConnectorDiff::Added {
                    connector: replacement,
                },
            ]
        );
    }

    #[test]
    fn exact_timing_change_is_not_collapsed_by_equal_rounded_refresh() {
        let original = connector(200, 31);
        let mut changed = original.clone();
        changed.modes[0].clock_khz += 1;
        assert_eq!(
            original.modes[0].refresh_millihz,
            changed.modes[0].refresh_millihz
        );

        assert!(matches!(
            diff_connector_scans(&scan([original]), &scan([changed])).as_slice(),
            [ConnectorDiff::Changed { .. }]
        ));
    }

    #[test]
    fn sysfs_connector_names_supply_stable_identity_and_raw_query_handles() {
        let root = temporary_sysfs_root("stable-identity");
        fs::create_dir_all(root.join("card1-DP-1")).expect("connector fixture");
        fs::create_dir_all(root.join("card1-DP-2")).expect("connector fixture");
        fs::write(root.join("card1-DP-1/connector_id"), "42\n").expect("connector id");
        fs::write(root.join("card1-DP-2/connector_id"), "7\n").expect("connector id");

        let handles =
            sysfs_connector_handles(Path::new("/dev/dri/card1"), &root).expect("sysfs scan");
        fs::remove_dir_all(&root).expect("remove fixture");

        assert_eq!(handles, vec![("DP-1".into(), 42), ("DP-2".into(), 7)]);
    }

    #[test]
    fn non_primary_card_filename_is_the_sole_sysfs_enumeration_falsifier() {
        let root = temporary_sysfs_root("card-name");
        fs::create_dir_all(&root).expect("fixture root");
        let error = sysfs_connector_handles(Path::new("/dev/dri/renderD128"), &root)
            .expect_err("render nodes are not primary card identities");
        fs::remove_dir_all(&root).expect("remove fixture");

        assert!(matches!(error, ConnectorScanError::CardName { .. }));
    }

    #[test]
    fn zero_connector_id_is_the_sole_sysfs_identity_falsifier() {
        let root = temporary_sysfs_root("zero-id");
        fs::create_dir_all(root.join("card1-DP-1")).expect("connector fixture");
        fs::write(root.join("card1-DP-1/connector_id"), "0\n").expect("connector id");

        let error = sysfs_connector_handles(Path::new("/dev/dri/card1"), &root)
            .expect_err("zero is not a DRM object handle");
        fs::remove_dir_all(&root).expect("remove fixture");

        assert!(matches!(
            error,
            ConnectorScanError::SysfsConnectorId { source, .. } if source == "connector id is zero"
        ));
    }

    #[test]
    fn connector_query_name_mismatch_is_the_sole_identity_falsifier() {
        assert!(connector_identity_matches("DP-1", "DP-1"));
        assert!(!connector_identity_matches("DP-1", "DP-2"));
    }

    #[test]
    fn auth_magic_success_reports_retained_implicit_master() {
        assert_eq!(
            accept_master_check_result(Ok(())).expect("authority check succeeded"),
            DrmMasterState::RetainedImplicit
        );
    }

    #[test]
    fn auth_magic_eacces_reports_non_master_without_acquiring_master() {
        assert_eq!(
            accept_master_check_result(Err(Errno::ACCESS))
                .expect("EACCES proves this fd is not current master"),
            DrmMasterState::NotMaster
        );
    }

    #[test]
    fn auth_magic_einval_reports_retained_implicit_master() {
        assert_eq!(
            accept_master_check_result(Err(Errno::INVAL))
                .expect("EINVAL proves authority check reached token lookup"),
            DrmMasterState::RetainedImplicit
        );
    }

    #[test]
    fn unexpected_master_check_failure_blocks_queries_as_the_sole_falsifier() {
        let error = accept_master_check_result(Err(Errno::IO))
            .expect_err("unexpected authority-check failure must stop the scan");
        assert_eq!(error.raw_os_error(), Some(Errno::IO.raw_os_error()));
    }

    #[test]
    fn path_dev_t_identity_is_enforced_before_any_drm_ioctl() {
        let path = Path::new("/dev/null");
        let actual = std::fs::metadata(path).expect("/dev/null metadata").rdev();
        let expected = actual.wrapping_add(1);
        let error = open_card_with(expected, path, |path| File::open(path))
            .expect_err("mismatched device identity must fail");
        assert!(matches!(
            error,
            ConnectorScanError::DeviceIdentity {
                expected: found_expected,
                actual: found_actual,
                ..
            } if found_expected == expected && found_actual == actual
        ));
    }

    fn temporary_sysfs_root(label: &str) -> PathBuf {
        static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "cosmix-comp-{label}-{}-{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ))
    }
}
