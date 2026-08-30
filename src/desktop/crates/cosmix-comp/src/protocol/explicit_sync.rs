use std::{
    fs::{File, OpenOptions},
    os::{
        fd::OwnedFd,
        unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
};

use cosmix_wgpu_dmabuf::VulkanDrmAdapter;
use smithay::{
    backend::drm::{CreateDrmNodeError, DrmDeviceFd, DrmNode, NodeType},
    utils::DeviceFd,
    wayland::drm_syncobj::supports_syncobj_eventfd,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct OpenRequest {
    read: bool,
    write: bool,
    close_on_exec: bool,
    create: bool,
    truncate: bool,
}

impl OpenRequest {
    const RENDER_NODE: Self = Self {
        read: true,
        write: true,
        close_on_exec: true,
        create: false,
        truncate: false,
    };
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ResolvedNode {
    path: PathBuf,
    node_type: NodeType,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct OpenedNode {
    is_character_device: bool,
    st_rdev: u64,
    node_type: Option<NodeType>,
}

struct ValidatedRenderFd<Fd> {
    fd: Fd,
}

pub(super) struct PreparedImportDevice<Device> {
    pub(super) device: Device,
    pub(super) expected_render_dev_t: u64,
    pub(super) observed_render_dev_t: u64,
    pub(super) resolved_path: PathBuf,
    pub(super) observed_node_type: NodeType,
}

impl<Device> PreparedImportDevice<Device> {
    /// Separate the device from the description of which device it is.
    ///
    /// The device goes to the syncobj state, which is the only thing that can
    /// use it; the identity goes into the startup report, which outlives the
    /// preparation and must not keep a DRM file descriptor open to say what was
    /// prepared.
    pub(super) fn split(self) -> (Device, PreparedImportDeviceIdentity) {
        let Self {
            device,
            expected_render_dev_t,
            observed_render_dev_t,
            resolved_path,
            observed_node_type,
        } = self;
        (
            device,
            PreparedImportDeviceIdentity {
                expected_render_dev_t,
                observed_render_dev_t,
                resolved_path,
                observed_node_type,
            },
        )
    }
}

/// Which device was prepared, in the terms the preparation itself checked.
///
/// Every field is one the preparation compared or resolved on the way to
/// deciding, so a report carrying this says which node was opened and on what
/// evidence — not merely that something was.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PreparedImportDeviceIdentity {
    pub(crate) expected_render_dev_t: u64,
    pub(crate) observed_render_dev_t: u64,
    pub(crate) resolved_path: PathBuf,
    pub(crate) observed_node_type: NodeType,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ImportDeviceUnavailable {
    MissingRenderNode,
    ResolveFailed(String),
    ResolvedNodeNotRender(NodeType),
    OpenFailed(String),
    InspectFailed(String),
    NotCharacterDevice(u64),
    NotDrmNode(u64),
    OpenedNodeNotRender(NodeType),
    RenderDeviceMismatch { expected: u64, observed: u64 },
    SyncobjEventfdUnsupported,
}

pub(super) enum ImportDeviceDecision<Device> {
    Prepared(PreparedImportDevice<Device>),
    Unavailable(ImportDeviceUnavailable),
}

/// What the protocol thread decided about the explicit-sync import device while
/// starting, in the thread's own words.
///
/// [`ImportDeviceDecision`] answers only the two questions preparation can
/// answer, and only while the device it carries is still in hand. This is the
/// same answer without the device, plus the third case preparation never sees:
/// the exposure mode said not to ask. Keeping all three distinct is the point —
/// the runtime used to reduce them to a log line and an `Option`, so a caller
/// that found no global could not tell a mode that withheld it from a device
/// that could not carry it, and had to re-probe the device afterwards to guess.
/// A re-probe samples a later world than the one that decided.
///
/// This describes **startup only**. A permanent fault later withdraws the
/// advertised global without changing what startup found, and this value does
/// not move when it does.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ExplicitSyncPreparation {
    /// The exposure mode does not prepare an import device, so none was asked
    /// for. No device was opened and no decision was reached.
    SkippedByPolicy,
    /// A device was prepared, and this is which one.
    Prepared(PreparedImportDeviceIdentity),
    /// Preparation ran and refused, for this reason.
    Unavailable(ImportDeviceUnavailable),
}

trait SyncobjPlatform {
    type OpenFd;
    type Device;

    fn resolve(&mut self, render_dev_t: u64) -> Result<ResolvedNode, String>;
    fn open(&mut self, path: &Path, request: OpenRequest) -> Result<Self::OpenFd, String>;
    fn inspect(&mut self, fd: &Self::OpenFd) -> Result<OpenedNode, String>;
    fn construct_import_device(
        &mut self,
        validated: ValidatedRenderFd<Self::OpenFd>,
    ) -> Self::Device;
    fn supports_eventfd(&mut self, device: &Self::Device) -> bool;
}

fn prepare_import_device<P: SyncobjPlatform>(
    selected_adapter: &VulkanDrmAdapter,
    platform: &mut P,
) -> ImportDeviceDecision<P::Device> {
    let Some(expected_render_dev_t) = selected_adapter.render_device else {
        return ImportDeviceDecision::Unavailable(ImportDeviceUnavailable::MissingRenderNode);
    };
    let resolved = match platform.resolve(expected_render_dev_t) {
        Ok(resolved) => resolved,
        Err(error) => {
            return ImportDeviceDecision::Unavailable(ImportDeviceUnavailable::ResolveFailed(
                error,
            ));
        }
    };
    if resolved.node_type != NodeType::Render {
        return ImportDeviceDecision::Unavailable(ImportDeviceUnavailable::ResolvedNodeNotRender(
            resolved.node_type,
        ));
    }

    let fd = match platform.open(&resolved.path, OpenRequest::RENDER_NODE) {
        Ok(fd) => fd,
        Err(error) => {
            return ImportDeviceDecision::Unavailable(ImportDeviceUnavailable::OpenFailed(error));
        }
    };
    let opened = match platform.inspect(&fd) {
        Ok(opened) => opened,
        Err(error) => {
            return ImportDeviceDecision::Unavailable(ImportDeviceUnavailable::InspectFailed(
                error,
            ));
        }
    };

    tracing::info!(
        expected_render_dev_t,
        observed_st_rdev = opened.st_rdev,
        resolved_path = %resolved.path.display(),
        observed_node_type = ?opened.node_type,
        "inspected explicit-sync DRM import device"
    );

    if !opened.is_character_device {
        return ImportDeviceDecision::Unavailable(ImportDeviceUnavailable::NotCharacterDevice(
            opened.st_rdev,
        ));
    }
    let Some(opened_node_type) = opened.node_type else {
        return ImportDeviceDecision::Unavailable(ImportDeviceUnavailable::NotDrmNode(
            opened.st_rdev,
        ));
    };
    if opened_node_type != NodeType::Render {
        return ImportDeviceDecision::Unavailable(ImportDeviceUnavailable::OpenedNodeNotRender(
            opened_node_type,
        ));
    }
    if opened.st_rdev != expected_render_dev_t {
        return ImportDeviceDecision::Unavailable(ImportDeviceUnavailable::RenderDeviceMismatch {
            expected: expected_render_dev_t,
            observed: opened.st_rdev,
        });
    }

    let device = platform.construct_import_device(ValidatedRenderFd { fd });
    if !platform.supports_eventfd(&device) {
        return ImportDeviceDecision::Unavailable(
            ImportDeviceUnavailable::SyncobjEventfdUnsupported,
        );
    }

    ImportDeviceDecision::Prepared(PreparedImportDevice {
        device,
        expected_render_dev_t,
        observed_render_dev_t: opened.st_rdev,
        resolved_path: resolved.path,
        observed_node_type: opened_node_type,
    })
}

pub(super) struct LinuxSyncobjPlatform;

impl SyncobjPlatform for LinuxSyncobjPlatform {
    type OpenFd = File;
    type Device = DrmDeviceFd;

    fn resolve(&mut self, render_dev_t: u64) -> Result<ResolvedNode, String> {
        let node = DrmNode::from_dev_id(render_dev_t as libc::dev_t).map_err(|error| {
            format!("failed to identify DRM render dev_t {render_dev_t}: {error}")
        })?;
        let path = node
            .dev_path()
            .ok_or_else(|| format!("DRM render dev_t {render_dev_t} has no device path"))?;
        Ok(ResolvedNode {
            path,
            node_type: node.ty(),
        })
    }

    fn open(&mut self, path: &Path, request: OpenRequest) -> Result<Self::OpenFd, String> {
        if request != OpenRequest::RENDER_NODE {
            return Err("refused unsafe DRM render-node open request".into());
        }
        OpenOptions::new()
            .read(request.read)
            .write(request.write)
            .create(request.create)
            .truncate(request.truncate)
            .custom_flags(if request.close_on_exec {
                libc::O_CLOEXEC
            } else {
                0
            })
            .open(path)
            .map_err(|error| format!("failed to open {}: {error}", path.display()))
    }

    fn inspect(&mut self, fd: &Self::OpenFd) -> Result<OpenedNode, String> {
        let metadata = fd
            .metadata()
            .map_err(|error| format!("failed to fstat DRM import device: {error}"))?;
        let node_type = match DrmNode::from_file(fd) {
            Ok(node) => Some(node.ty()),
            Err(CreateDrmNodeError::NotDrmNode) => None,
            Err(CreateDrmNodeError::Io(error)) => {
                return Err(format!("failed to inspect DRM import device: {error}"));
            }
        };
        Ok(OpenedNode {
            is_character_device: metadata.file_type().is_char_device(),
            st_rdev: metadata.rdev(),
            node_type,
        })
    }

    fn construct_import_device(
        &mut self,
        validated: ValidatedRenderFd<Self::OpenFd>,
    ) -> Self::Device {
        let owned_fd: OwnedFd = validated.fd.into();
        DrmDeviceFd::new(DeviceFd::from(owned_fd))
    }

    fn supports_eventfd(&mut self, device: &Self::Device) -> bool {
        supports_syncobj_eventfd(device)
    }
}

pub(super) fn prepare_linux_import_device(
    selected_adapter: &VulkanDrmAdapter,
) -> ImportDeviceDecision<DrmDeviceFd> {
    prepare_import_device(selected_adapter, &mut LinuxSyncobjPlatform)
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXPECTED_RENDER_DEV_T: u64 = 0x0102_0304;
    const OTHER_RENDER_DEV_T: u64 = 0x0506_0708;
    const FAKE_PATH: &str = "/synthetic/render-node";

    #[derive(Clone, Debug, Eq, PartialEq)]
    enum PlatformCall {
        Resolve(u64),
        Open { path: PathBuf, request: OpenRequest },
        Inspect,
        Construct,
        ProbeEventfd,
    }

    struct FakeFd;
    struct FakeDevice;

    struct FakePlatform {
        resolve_result: Option<Result<ResolvedNode, String>>,
        open_result: Option<Result<FakeFd, String>>,
        inspect_result: Option<Result<OpenedNode, String>>,
        supports_eventfd: bool,
        calls: Vec<PlatformCall>,
    }

    impl FakePlatform {
        fn ready() -> Self {
            Self {
                resolve_result: Some(Ok(ResolvedNode {
                    path: FAKE_PATH.into(),
                    node_type: NodeType::Render,
                })),
                open_result: Some(Ok(FakeFd)),
                inspect_result: Some(Ok(OpenedNode {
                    is_character_device: true,
                    st_rdev: EXPECTED_RENDER_DEV_T,
                    node_type: Some(NodeType::Render),
                })),
                supports_eventfd: true,
                calls: Vec::new(),
            }
        }

        fn call_count(&self, expected: PlatformCall) -> usize {
            self.calls.iter().filter(|call| **call == expected).count()
        }
    }

    impl SyncobjPlatform for FakePlatform {
        type OpenFd = FakeFd;
        type Device = FakeDevice;

        fn resolve(&mut self, render_dev_t: u64) -> Result<ResolvedNode, String> {
            self.calls.push(PlatformCall::Resolve(render_dev_t));
            self.resolve_result
                .take()
                .expect("fake resolve result is scripted")
        }

        fn open(&mut self, path: &Path, request: OpenRequest) -> Result<Self::OpenFd, String> {
            self.calls.push(PlatformCall::Open {
                path: path.to_owned(),
                request,
            });
            self.open_result
                .take()
                .expect("fake open result is scripted")
        }

        fn inspect(&mut self, _fd: &Self::OpenFd) -> Result<OpenedNode, String> {
            self.calls.push(PlatformCall::Inspect);
            self.inspect_result
                .take()
                .expect("fake inspection result is scripted")
        }

        fn construct_import_device(
            &mut self,
            validated: ValidatedRenderFd<Self::OpenFd>,
        ) -> Self::Device {
            let ValidatedRenderFd { fd: FakeFd } = validated;
            self.calls.push(PlatformCall::Construct);
            FakeDevice
        }

        fn supports_eventfd(&mut self, _device: &Self::Device) -> bool {
            self.calls.push(PlatformCall::ProbeEventfd);
            self.supports_eventfd
        }
    }

    fn adapter(primary_device: Option<u64>, render_device: Option<u64>) -> VulkanDrmAdapter {
        VulkanDrmAdapter {
            name: "Synthetic adapter".into(),
            device_type: "TEST".into(),
            primary_device,
            render_device,
        }
    }

    fn assert_unavailable(
        decision: ImportDeviceDecision<FakeDevice>,
        expected: ImportDeviceUnavailable,
    ) {
        let ImportDeviceDecision::Unavailable(actual) = decision else {
            panic!("expected import device to be unavailable");
        };
        assert_eq!(actual, expected);
    }

    #[test]
    fn primary_only_adapter_never_resolves_a_node() {
        let mut platform = FakePlatform::ready();
        let decision =
            prepare_import_device(&adapter(Some(EXPECTED_RENDER_DEV_T), None), &mut platform);

        assert_unavailable(decision, ImportDeviceUnavailable::MissingRenderNode);
        assert!(platform.calls.is_empty());
    }

    #[test]
    fn resolved_primary_identity_never_opens() {
        let mut platform = FakePlatform::ready();
        platform.resolve_result = Some(Ok(ResolvedNode {
            path: FAKE_PATH.into(),
            node_type: NodeType::Primary,
        }));

        let decision =
            prepare_import_device(&adapter(None, Some(EXPECTED_RENDER_DEV_T)), &mut platform);

        assert_unavailable(
            decision,
            ImportDeviceUnavailable::ResolvedNodeNotRender(NodeType::Primary),
        );
        assert_eq!(
            platform.calls,
            [PlatformCall::Resolve(EXPECTED_RENDER_DEV_T)]
        );
    }

    #[test]
    fn render_node_open_uses_exact_safe_flags() {
        let mut platform = FakePlatform::ready();

        let _ = prepare_import_device(&adapter(None, Some(EXPECTED_RENDER_DEV_T)), &mut platform);

        let PlatformCall::Open { request, .. } = platform
            .calls
            .iter()
            .find(|call| matches!(call, PlatformCall::Open { .. }))
            .expect("render node is opened")
        else {
            unreachable!();
        };
        assert_eq!(*request, OpenRequest::RENDER_NODE);
        assert!(request.read);
        assert!(request.write);
        assert!(request.close_on_exec);
        assert!(!request.create);
        assert!(!request.truncate);
    }

    #[test]
    fn linux_platform_refuses_non_render_open_request_without_opening() {
        let mut platform = LinuxSyncobjPlatform;
        let unsafe_request = OpenRequest {
            write: false,
            ..OpenRequest::RENDER_NODE
        };

        let error = platform
            .open(Path::new(FAKE_PATH), unsafe_request)
            .expect_err("unsafe request must be rejected before opening its path");

        assert_eq!(error, "refused unsafe DRM render-node open request");
    }

    #[test]
    fn open_failure_never_inspects_or_constructs() {
        let mut platform = FakePlatform::ready();
        platform.open_result = Some(Err("synthetic open failure".into()));

        let decision =
            prepare_import_device(&adapter(None, Some(EXPECTED_RENDER_DEV_T)), &mut platform);

        assert_unavailable(
            decision,
            ImportDeviceUnavailable::OpenFailed("synthetic open failure".into()),
        );
        assert_eq!(platform.call_count(PlatformCall::Inspect), 0);
        assert_eq!(platform.call_count(PlatformCall::Construct), 0);
    }

    #[test]
    fn non_character_fd_never_constructs() {
        let mut platform = FakePlatform::ready();
        platform.inspect_result = Some(Ok(OpenedNode {
            is_character_device: false,
            st_rdev: EXPECTED_RENDER_DEV_T,
            node_type: Some(NodeType::Render),
        }));

        let decision =
            prepare_import_device(&adapter(None, Some(EXPECTED_RENDER_DEV_T)), &mut platform);

        assert_unavailable(
            decision,
            ImportDeviceUnavailable::NotCharacterDevice(EXPECTED_RENDER_DEV_T),
        );
        assert_eq!(platform.call_count(PlatformCall::Construct), 0);
    }

    #[test]
    fn non_drm_character_fd_never_constructs() {
        let mut platform = FakePlatform::ready();
        platform.inspect_result = Some(Ok(OpenedNode {
            is_character_device: true,
            st_rdev: EXPECTED_RENDER_DEV_T,
            node_type: None,
        }));

        let decision =
            prepare_import_device(&adapter(None, Some(EXPECTED_RENDER_DEV_T)), &mut platform);

        assert_unavailable(
            decision,
            ImportDeviceUnavailable::NotDrmNode(EXPECTED_RENDER_DEV_T),
        );
        assert_eq!(platform.call_count(PlatformCall::Construct), 0);
    }

    #[test]
    fn opened_primary_node_never_reaches_device_construction() {
        let mut platform = FakePlatform::ready();
        platform.inspect_result = Some(Ok(OpenedNode {
            is_character_device: true,
            st_rdev: EXPECTED_RENDER_DEV_T,
            node_type: Some(NodeType::Primary),
        }));

        let decision =
            prepare_import_device(&adapter(None, Some(EXPECTED_RENDER_DEV_T)), &mut platform);

        assert_unavailable(
            decision,
            ImportDeviceUnavailable::OpenedNodeNotRender(NodeType::Primary),
        );
        assert_eq!(platform.call_count(PlatformCall::Construct), 0);
        assert_eq!(platform.call_count(PlatformCall::ProbeEventfd), 0);
    }

    #[test]
    fn different_render_node_never_reaches_device_construction() {
        let mut platform = FakePlatform::ready();
        platform.inspect_result = Some(Ok(OpenedNode {
            is_character_device: true,
            st_rdev: OTHER_RENDER_DEV_T,
            node_type: Some(NodeType::Render),
        }));

        let decision =
            prepare_import_device(&adapter(None, Some(EXPECTED_RENDER_DEV_T)), &mut platform);

        assert_unavailable(
            decision,
            ImportDeviceUnavailable::RenderDeviceMismatch {
                expected: EXPECTED_RENDER_DEV_T,
                observed: OTHER_RENDER_DEV_T,
            },
        );
        assert_eq!(platform.call_count(PlatformCall::Construct), 0);
        assert_eq!(platform.call_count(PlatformCall::ProbeEventfd), 0);
    }

    #[test]
    fn matching_render_node_constructs_once_after_all_inspections() {
        let mut platform = FakePlatform::ready();

        let decision =
            prepare_import_device(&adapter(None, Some(EXPECTED_RENDER_DEV_T)), &mut platform);

        assert!(matches!(decision, ImportDeviceDecision::Prepared(_)));
        assert_eq!(
            platform.calls,
            [
                PlatformCall::Resolve(EXPECTED_RENDER_DEV_T),
                PlatformCall::Open {
                    path: FAKE_PATH.into(),
                    request: OpenRequest::RENDER_NODE,
                },
                PlatformCall::Inspect,
                PlatformCall::Construct,
                PlatformCall::ProbeEventfd,
            ]
        );
    }

    #[test]
    fn unsupported_eventfd_returns_unavailable() {
        let mut platform = FakePlatform::ready();
        platform.supports_eventfd = false;

        let decision =
            prepare_import_device(&adapter(None, Some(EXPECTED_RENDER_DEV_T)), &mut platform);

        assert_unavailable(decision, ImportDeviceUnavailable::SyncobjEventfdUnsupported);
        assert_eq!(platform.call_count(PlatformCall::Construct), 1);
        assert_eq!(platform.call_count(PlatformCall::ProbeEventfd), 1);
    }

    #[test]
    #[ignore = "opens the DRM render node named by COSMIX_TEST_RENDER_NODE"]
    fn real_render_node_prepares_linux_import_device() {
        const RENDER_NODE_ENV: &str = "COSMIX_TEST_RENDER_NODE";

        let supplied_path = PathBuf::from(
            std::env::var_os(RENDER_NODE_ENV)
                .unwrap_or_else(|| panic!("{RENDER_NODE_ENV} must name a DRM render node")),
        );
        let supplied_node = DrmNode::from_path(&supplied_path).unwrap_or_else(|error| {
            panic!(
                "{} must identify a DRM render node: {error}",
                supplied_path.display()
            )
        });
        assert_eq!(
            supplied_node.ty(),
            NodeType::Render,
            "refusing to run against non-render DRM node {}",
            supplied_path.display()
        );

        let expected_render_dev_t = supplied_node.dev_id() as u64;
        let selected_adapter = VulkanDrmAdapter {
            name: format!("ignored hardware test ({})", supplied_path.display()),
            device_type: "TEST-HARDWARE".into(),
            primary_device: None,
            render_device: Some(expected_render_dev_t),
        };

        let prepared = match prepare_linux_import_device(&selected_adapter) {
            ImportDeviceDecision::Prepared(prepared) => prepared,
            ImportDeviceDecision::Unavailable(reason) => {
                panic!("real render-node preparation was unavailable: {reason:?}")
            }
        };

        assert_eq!(prepared.expected_render_dev_t, expected_render_dev_t);
        assert_eq!(prepared.observed_render_dev_t, expected_render_dev_t);
        assert_eq!(prepared.observed_node_type, NodeType::Render);
        let resolved_node = DrmNode::from_path(&prepared.resolved_path)
            .expect("prepared path must still identify a DRM node");
        assert_eq!(
            resolved_node, supplied_node,
            "resolved path must identify the explicitly supplied render node"
        );
    }
}
