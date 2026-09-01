//! Vulkan DMA-BUF import for Bevy 0.19 and wgpu 29.
//!
//! The public boundary intentionally contains only owned file descriptors,
//! DRM metadata and Bevy image handles. All Vulkan and wgpu-hal ownership is
//! private to this crate.

mod drm;
mod formats;
mod import;
mod init;
mod retirement;
mod scanout;

use std::os::fd::{AsFd, OwnedFd};

pub use drm::{VulkanDrmAdapter, VulkanDrmProbe};
pub use import::{
    DmabufImportPlugin, DmabufMaterial2dRegistrationExt, DmabufProbePlugin, DmabufRelease,
    DmabufValidator, ImportedDmabufImages, ValidateDmabuf, dmabuf_material_prepare_barrier,
};
pub use init::{ManualVulkanRenderer, ScanoutProbeRenderer};
pub use retirement::{
    RETIREMENT_WAIT_TIMEOUT, RetirementBatchId, RetirementRequestError, RetirementRequestSender,
    RetirementSequence, RetirementWaitError, RetirementWorker, RetirementWorkerError,
    RetirementWorkerReport, WaitForSubmittedWork, WgpuWaitForSubmittedWork,
    spawn_retirement_worker,
};
pub use scanout::{
    CaptureDestinationBridge, CaptureDestinationCapabilities, CaptureDestinationError,
    CaptureDestinationSupport, CaptureDestinationWgpuFormat, ImportedCaptureDestination,
    ScanoutCapabilityError, ScanoutImportCapabilities, ScanoutImportError, ScanoutImportSupport,
    ScanoutRenderBridge, ScanoutRenderTarget, ScanoutWgpuFormat, capture_destination_wgpu_format,
    is_opaque_scanout_format, preferred_scanout_fourccs, scanout_wgpu_format,
};

/// Current implementation status, exposed in compositor startup diagnostics.
pub const BRIDGE_STATUS: &str = "rung-d-vulkan-dmabuf";

/// Renderer cache identity for one Wayland `wl_buffer` resource.
///
/// The compositor allocates this independently of protocol object numbers, so
/// a client may reuse a destroyed object number without aliasing an old import.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct DmabufBufferId(pub u64);

/// A DRM format/modifier pair confirmed importable by the selected Vulkan GPU.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct DmabufFormat {
    pub fourcc: u32,
    pub modifier: u64,
    /// Number of DRM memory planes described by this modifier.
    pub plane_count: u32,
}

/// DMA-BUF capabilities used to construct Smithay's v4 feedback table.
#[derive(Clone, Debug)]
pub struct DmabufCapabilities {
    /// Linux `dev_t` for the Vulkan physical device's render or primary node.
    pub main_device: u64,
    pub formats: Vec<DmabufFormat>,
    pub adapter_name: String,
    /// DRM identity queried from the exact Vulkan physical device used by wgpu.
    pub drm_adapter: VulkanDrmAdapter,
}

/// One owned DMA-BUF memory plane.
#[derive(Debug)]
pub struct DmabufPlane {
    pub fd: OwnedFd,
    pub offset: u32,
    pub stride: u32,
}

/// Fully owned metadata required to import one client buffer.
#[derive(Debug)]
pub struct DmabufDescriptor {
    pub width: u32,
    pub height: u32,
    pub fourcc: u32,
    pub modifier: u64,
    pub planes: Vec<DmabufPlane>,
}

impl DmabufDescriptor {
    /// Whether the DRM format's fourth channel is padding rather than alpha.
    pub fn is_opaque(&self) -> bool {
        formats::is_opaque(self.fourcc)
    }

    /// Duplicate every plane FD so the same authoritative buffer description
    /// can be handed to another bounded import attempt.
    pub fn try_clone(&self) -> std::io::Result<Self> {
        Ok(Self {
            width: self.width,
            height: self.height,
            fourcc: self.fourcc,
            modifier: self.modifier,
            planes: self
                .planes
                .iter()
                .map(|plane| {
                    Ok(DmabufPlane {
                        fd: plane.fd.as_fd().try_clone_to_owned()?,
                        offset: plane.offset,
                        stride: plane.stride,
                    })
                })
                .collect::<std::io::Result<Vec<_>>>()?,
        })
    }
}

/// Called when the selected DMA-BUF ownership lifetime ends.
pub type ReleaseCallback = Box<dyn FnOnce() + Send + Sync + 'static>;
