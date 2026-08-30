//! Read-only Vulkan capability queries for compositor-owned scanout buffers.
//!
//! This is deliberately separate from `formats.rs`' client DMA-BUF feedback
//! contract: client textures require sampled-image import, while compositor
//! scanout buffers require external-memory import as colour attachments.

use ash::vk;
use bevy::{
    math::UVec2,
    render::{
        render_resource::{Texture, TextureView},
        renderer::{RenderAdapter, RenderDevice, RenderInstance, RenderQueue},
        texture::ManualTextureView,
    },
};
use drm_fourcc::DrmFourcc;
use thiserror::Error;
use wgpu_hal::{
    MemoryFlags, TextureDescriptor as HalTextureDescriptor, api::Vulkan, vulkan::TextureMemory,
};
use wgpu_types::TextureUses;

use crate::formats::{
    drm_modifier_properties, drm_to_vulkan, external_import_properties_with_usage,
    external_import_properties_with_usage_and_view_formats, is_opaque, vulkan_to_wgpu,
};
use crate::{DmabufDescriptor, WgpuWaitForSubmittedWork, import::import_vulkan_image};

const BGRA8_SRGB_VIEW_FORMATS: &[wgpu::TextureFormat] = &[wgpu::TextureFormat::Bgra8UnormSrgb];
const RGBA8_SRGB_VIEW_FORMATS: &[wgpu::TextureFormat] = &[wgpu::TextureFormat::Rgba8UnormSrgb];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScanoutWgpuFormat {
    Bgra8Unorm,
    Rgba8Unorm,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScanoutImportSupport {
    pub wgpu_format: Option<ScanoutWgpuFormat>,
    pub vulkan_external_memory_colour_attachment: bool,
    pub vulkan_external_memory_transfer_src: bool,
    pub mode_extent_supported: bool,
    pub max_extent: Option<(u32, u32)>,
}

impl ScanoutImportSupport {
    pub fn supported(self) -> bool {
        self.wgpu_format.is_some()
            && self.vulkan_external_memory_colour_attachment
            && self.vulkan_external_memory_transfer_src
            && self.mode_extent_supported
    }
}

#[derive(Debug, Error)]
pub enum ScanoutCapabilityError {
    #[error("manual renderer is not backed by Vulkan")]
    NotVulkan,
}

#[derive(Debug, Error)]
pub enum ScanoutImportError {
    #[error("scanout DMA-BUF belongs to DRM device {supplied}, renderer uses {renderer}")]
    DeviceMismatch { supplied: u64, renderer: u64 },
    #[error("scanout DMA-BUF import failed: {0}")]
    Import(String),
}

/// Cloneable import boundary for compositor-allocated scanout buffers.
///
/// The bridge is created only by a renderer already pinned to `drm_device`.
/// Import repeats that identity check at the operation boundary so a resumed
/// or renumbered card cannot accidentally feed another GPU's allocation into
/// this Vulkan device.
#[derive(Clone)]
pub struct ScanoutRenderBridge {
    instance: RenderInstance,
    adapter: RenderAdapter,
    device: RenderDevice,
    queue: RenderQueue,
    drm_device: u64,
}

impl ScanoutRenderBridge {
    pub(crate) fn new(
        instance: RenderInstance,
        adapter: RenderAdapter,
        device: RenderDevice,
        queue: RenderQueue,
        drm_device: u64,
    ) -> Self {
        Self {
            instance,
            adapter,
            device,
            queue,
            drm_device,
        }
    }

    pub fn capabilities(&self) -> ScanoutImportCapabilities {
        ScanoutImportCapabilities::new(self.instance.clone(), self.adapter.clone())
    }

    pub fn import(
        &self,
        drm_device: u64,
        descriptor: DmabufDescriptor,
    ) -> Result<ScanoutRenderTarget, ScanoutImportError> {
        validate_renderer_device(self.drm_device, drm_device)?;
        import_scanout_target(&self.device, descriptor)
            .map_err(|error| ScanoutImportError::Import(error.to_string()))
    }

    /// A synchronous, deadline-bounded completion adapter for pool reuse.
    /// Calling it does not create a worker thread.
    pub fn retirement_adapter(&self) -> WgpuWaitForSubmittedWork {
        WgpuWaitForSubmittedWork::with_queue(self.device.clone(), self.queue.clone())
    }
}

fn validate_renderer_device(renderer: u64, supplied: u64) -> Result<(), ScanoutImportError> {
    if renderer == supplied {
        Ok(())
    } else {
        Err(ScanoutImportError::DeviceMismatch { supplied, renderer })
    }
}

/// One imported GBM allocation. The wgpu texture keeps the raw Vulkan image
/// and imported memory alive; the manual view is the Bevy camera target.
pub struct ScanoutRenderTarget {
    _texture: Texture,
    view: ManualTextureView,
}

impl ScanoutRenderTarget {
    pub fn manual_view(&self) -> ManualTextureView {
        self.view.clone()
    }
}

fn scanout_texture_descriptor(
    descriptor: &DmabufDescriptor,
) -> Result<wgpu::TextureDescriptor<'static>, crate::import::ImportError> {
    if descriptor.width == 0 || descriptor.height == 0 {
        return Err(crate::import::ImportError::InvalidDimensions);
    }
    if descriptor.planes.is_empty() {
        return Err(crate::import::ImportError::NoPlanes);
    }
    let vulkan_format = drm_to_vulkan(descriptor.fourcc).ok_or(
        crate::import::ImportError::UnsupportedFourcc(descriptor.fourcc),
    )?;
    let format = vulkan_to_wgpu(vulkan_format).ok_or(
        crate::import::ImportError::UnsupportedFourcc(descriptor.fourcc),
    )?;
    let view_formats = scanout_srgb_view_formats(format).ok_or(
        crate::import::ImportError::UnsupportedFourcc(descriptor.fourcc),
    )?;
    Ok(wgpu::TextureDescriptor {
        label: Some("CosMix GBM scanout target"),
        size: wgpu::Extent3d {
            width: descriptor.width,
            height: descriptor.height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats,
    })
}

fn import_scanout_target(
    render_device: &RenderDevice,
    descriptor: DmabufDescriptor,
) -> Result<ScanoutRenderTarget, crate::import::ImportError> {
    let wgpu_descriptor = scanout_texture_descriptor(&descriptor)?;
    let vulkan_format = drm_to_vulkan(descriptor.fourcc).ok_or(
        crate::import::ImportError::UnsupportedFourcc(descriptor.fourcc),
    )?;
    let srgb_view_format = scanout_srgb_view_format(wgpu_descriptor.format).ok_or(
        crate::import::ImportError::UnsupportedFourcc(descriptor.fourcc),
    )?;
    let vulkan_view_format = scanout_srgb_vulkan_view_format(vulkan_format).ok_or(
        crate::import::ImportError::UnsupportedFourcc(descriptor.fourcc),
    )?;
    let hal_descriptor = HalTextureDescriptor {
        label: wgpu_descriptor.label,
        size: wgpu_descriptor.size,
        mip_level_count: wgpu_descriptor.mip_level_count,
        sample_count: wgpu_descriptor.sample_count,
        dimension: wgpu_descriptor.dimension,
        format: wgpu_descriptor.format,
        usage: scanout_hal_usage(),
        memory_flags: MemoryFlags::empty(),
        view_formats: wgpu_descriptor.view_formats.to_vec(),
    };
    let render_device_for_drop = render_device.clone();
    let hal_texture = unsafe {
        let Some(device) = render_device.wgpu_device().as_hal::<Vulkan>() else {
            return Err(crate::import::ImportError::NotVulkan);
        };
        let (image, memories, _) = import_vulkan_image(
            &device,
            descriptor,
            vulkan_format,
            scanout_vulkan_usage(),
            &[vulkan_view_format, vulkan_format],
        )?;
        let drop_callback: wgpu_hal::DropCallback = Box::new(move || {
            // SAFETY: wgpu invokes the callback only after every texture/view
            // reference is gone; the captured RenderDevice keeps Vulkan alive.
            if let Some(device) = render_device_for_drop.wgpu_device().as_hal::<Vulkan>() {
                device.raw_device().destroy_image(image, None);
                for memory in memories {
                    device.raw_device().free_memory(memory, None);
                }
            }
        });
        device.texture_from_raw(
            image,
            &hal_descriptor,
            Some(drop_callback),
            TextureMemory::External,
        )
    };
    let (wgpu_texture, tracker_seed) = unsafe {
        // The GBM allocation has never been used by KMS or another queue. Its
        // Vulkan image starts UNDEFINED, so wgpu owns the first transition.
        render_device
            .wgpu_device()
            .create_texture_from_hal_with_initial_usage::<Vulkan>(
                hal_texture,
                &wgpu_descriptor,
                TextureUses::UNINITIALIZED,
            )
    };
    debug_assert_eq!(tracker_seed, TextureUses::UNINITIALIZED);
    let texture = Texture::from(wgpu_texture);
    let texture_view: TextureView = texture.create_view(&wgpu::TextureViewDescriptor {
        label: Some("CosMix GBM scanout sRGB render view"),
        format: Some(srgb_view_format),
        ..Default::default()
    });
    let view = ManualTextureView {
        texture_view,
        size: UVec2::new(wgpu_descriptor.size.width, wgpu_descriptor.size.height),
        view_format: srgb_view_format,
    };
    Ok(ScanoutRenderTarget {
        _texture: texture,
        view,
    })
}

fn scanout_hal_usage() -> TextureUses {
    TextureUses::COLOR_TARGET | TextureUses::COPY_SRC
}

fn scanout_vulkan_usage() -> vk::ImageUsageFlags {
    vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_SRC
}

fn scanout_srgb_view_formats(
    format: wgpu::TextureFormat,
) -> Option<&'static [wgpu::TextureFormat]> {
    match format {
        wgpu::TextureFormat::Bgra8Unorm => Some(BGRA8_SRGB_VIEW_FORMATS),
        wgpu::TextureFormat::Rgba8Unorm => Some(RGBA8_SRGB_VIEW_FORMATS),
        _ => None,
    }
}

fn scanout_srgb_view_format(format: wgpu::TextureFormat) -> Option<wgpu::TextureFormat> {
    scanout_srgb_view_formats(format).and_then(|formats| formats.first().copied())
}

fn scanout_srgb_vulkan_view_format(format: vk::Format) -> Option<vk::Format> {
    match format {
        vk::Format::B8G8R8A8_UNORM => Some(vk::Format::B8G8R8A8_SRGB),
        vk::Format::R8G8B8A8_UNORM => Some(vk::Format::R8G8B8A8_SRGB),
        _ => None,
    }
}

/// Capability-query half of the future offscreen scanout bridge.
///
/// It owns only cloneable wgpu handles and performs physical-device queries;
/// it allocates no image and submits no work.
#[derive(Clone)]
pub struct ScanoutImportCapabilities {
    instance: RenderInstance,
    adapter: RenderAdapter,
}

impl ScanoutImportCapabilities {
    pub(crate) fn new(instance: RenderInstance, adapter: RenderAdapter) -> Self {
        Self { instance, adapter }
    }

    pub fn modifiers_for(&self, fourcc: u32) -> Result<Vec<u64>, ScanoutCapabilityError> {
        self.with_vulkan(|instance, physical_device| {
            let Some(format) = drm_to_vulkan(fourcc) else {
                return Vec::new();
            };
            let mut modifiers = drm_modifier_properties(instance, physical_device, format)
                .into_iter()
                .map(|properties| properties.drm_format_modifier)
                .collect::<Vec<_>>();
            modifiers.sort_unstable();
            modifiers.dedup();
            modifiers
        })
    }

    pub fn query(
        &self,
        fourcc: u32,
        modifier: u64,
        width: u32,
        height: u32,
    ) -> Result<ScanoutImportSupport, ScanoutCapabilityError> {
        self.with_vulkan(|instance, physical_device| {
            query_vulkan_scanout_support(instance, physical_device, fourcc, modifier, width, height)
        })
    }

    fn with_vulkan<R>(
        &self,
        operation: impl FnOnce(&ash::Instance, vk::PhysicalDevice) -> R,
    ) -> Result<R, ScanoutCapabilityError> {
        let instance =
            unsafe { self.instance.as_hal::<Vulkan>() }.ok_or(ScanoutCapabilityError::NotVulkan)?;
        let adapter =
            unsafe { self.adapter.as_hal::<Vulkan>() }.ok_or(ScanoutCapabilityError::NotVulkan)?;
        Ok(operation(
            instance.shared_instance().raw_instance(),
            adapter.raw_physical_device(),
        ))
    }
}

fn query_vulkan_scanout_support(
    instance: &ash::Instance,
    physical_device: vk::PhysicalDevice,
    fourcc: u32,
    modifier: u64,
    width: u32,
    height: u32,
) -> ScanoutImportSupport {
    let Some(vulkan_format) = drm_to_vulkan(fourcc) else {
        return ScanoutImportSupport {
            wgpu_format: None,
            vulkan_external_memory_colour_attachment: false,
            vulkan_external_memory_transfer_src: false,
            mode_extent_supported: false,
            max_extent: None,
        };
    };
    let wgpu_format = scanout_wgpu_format(fourcc);
    let srgb_view_format = scanout_srgb_vulkan_view_format(vulkan_format);
    let modifier_support = drm_modifier_properties(instance, physical_device, vulkan_format)
        .into_iter()
        .find(|properties| properties.drm_format_modifier == modifier)
        .filter(|properties| properties.drm_format_modifier_plane_count == 1);
    let colour_attachment = modifier_support.is_some_and(|properties| {
        properties
            .drm_format_modifier_tiling_features
            .contains(vk::FormatFeatureFlags2::COLOR_ATTACHMENT)
            && external_import_properties_with_usage(
                instance,
                physical_device,
                vulkan_format,
                modifier,
                vk::ImageUsageFlags::COLOR_ATTACHMENT,
            )
            .is_some()
    });
    let render_and_copy = modifier_support
        .filter(|properties| {
            properties
                .drm_format_modifier_tiling_features
                .contains(vk::FormatFeatureFlags2::COLOR_ATTACHMENT)
        })
        .and(srgb_view_format)
        .and_then(|view_format| {
            external_import_properties_with_usage_and_view_formats(
                instance,
                physical_device,
                vulkan_format,
                modifier,
                required_scanout_usage(),
                &[view_format, vulkan_format],
            )
        });
    let max_extent = render_and_copy
        .map(|properties| (properties.max_extent.width, properties.max_extent.height));
    ScanoutImportSupport {
        wgpu_format,
        vulkan_external_memory_colour_attachment: colour_attachment,
        vulkan_external_memory_transfer_src: render_and_copy.is_some(),
        mode_extent_supported: max_extent
            .is_some_and(|extent| mode_fits_extent(width, height, extent)),
        max_extent,
    }
}

fn required_scanout_usage() -> vk::ImageUsageFlags {
    vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_SRC
}

fn mode_fits_extent(width: u32, height: u32, max_extent: (u32, u32)) -> bool {
    width <= max_extent.0 && height <= max_extent.1
}

pub fn scanout_wgpu_format(fourcc: u32) -> Option<ScanoutWgpuFormat> {
    let vulkan = drm_to_vulkan(fourcc)?;
    match vulkan_to_wgpu(vulkan)? {
        // XR24's X byte is still written by wgpu's BGRA target, but an opaque
        // primary-plane format ignores it. This mapping is safe for selection
        // only while atomic admission separately enforces the opaque policy.
        wgpu::TextureFormat::Bgra8Unorm => Some(ScanoutWgpuFormat::Bgra8Unorm),
        wgpu::TextureFormat::Rgba8Unorm => Some(ScanoutWgpuFormat::Rgba8Unorm),
        _ => None,
    }
}

pub fn is_opaque_scanout_format(fourcc: u32) -> bool {
    is_opaque(fourcc)
}

pub fn preferred_scanout_fourccs() -> [u32; 2] {
    [DrmFourcc::Xrgb8888 as u32, DrmFourcc::Xbgr8888 as u32]
}

#[cfg(test)]
mod tests {
    use std::{os::unix::net::UnixStream, sync::mpsc, time::Duration};

    use super::*;
    use crate::DmabufPlane;

    fn descriptor(fourcc: DrmFourcc) -> DmabufDescriptor {
        DmabufDescriptor {
            width: 1920,
            height: 1080,
            fourcc: fourcc as u32,
            modifier: 0,
            planes: vec![DmabufPlane {
                fd: UnixStream::pair().expect("test descriptor fd").0.into(),
                offset: 0,
                stride: 1920 * 4,
            }],
        }
    }

    #[test]
    fn xrgb_and_xbgr_map_to_the_required_wgpu_colour_attachments() {
        assert_eq!(
            scanout_wgpu_format(DrmFourcc::Xrgb8888 as u32),
            Some(ScanoutWgpuFormat::Bgra8Unorm)
        );
        assert_eq!(
            scanout_wgpu_format(DrmFourcc::Xbgr8888 as u32),
            Some(ScanoutWgpuFormat::Rgba8Unorm)
        );
        assert_eq!(scanout_wgpu_format(DrmFourcc::Nv12 as u32), None);
    }

    #[test]
    fn opaque_scanout_ranking_is_xrgb_then_xbgr() {
        assert_eq!(
            preferred_scanout_fourccs(),
            [DrmFourcc::Xrgb8888 as u32, DrmFourcc::Xbgr8888 as u32]
        );
    }

    #[test]
    fn scanout_usage_includes_capture_transfer_source() {
        let usage = required_scanout_usage();
        assert!(usage.contains(vk::ImageUsageFlags::COLOR_ATTACHMENT));
        assert!(usage.contains(vk::ImageUsageFlags::TRANSFER_SRC));
    }

    #[test]
    fn preferred_mode_must_fit_the_reported_vulkan_max_extent() {
        assert!(mode_fits_extent(1920, 1080, (1920, 1080)));
        assert!(!mode_fits_extent(1920, 1080, (1280, 1080)));
        assert!(!mode_fits_extent(1920, 1080, (1920, 720)));
    }

    #[test]
    fn scanout_import_contract_maps_xrgb_to_bgra_render_and_capture_usage() {
        let descriptor = scanout_texture_descriptor(&descriptor(DrmFourcc::Xrgb8888))
            .expect("XR24 scanout descriptor");
        assert_eq!(descriptor.format, wgpu::TextureFormat::Bgra8Unorm);
        assert_eq!(
            descriptor.view_formats,
            &[wgpu::TextureFormat::Bgra8UnormSrgb]
        );
        assert_eq!(
            descriptor.usage,
            wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC
        );
    }

    #[test]
    fn scanout_import_contract_maps_xbgr_to_rgba_render_and_capture_usage() {
        let descriptor = scanout_texture_descriptor(&descriptor(DrmFourcc::Xbgr8888))
            .expect("XB24 scanout descriptor");
        assert_eq!(descriptor.format, wgpu::TextureFormat::Rgba8Unorm);
        assert_eq!(
            descriptor.view_formats,
            &[wgpu::TextureFormat::Rgba8UnormSrgb]
        );
        assert_eq!(
            descriptor.usage,
            wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC
        );
    }

    #[test]
    fn scanout_import_contract_is_render_and_capture_at_every_api_layer() {
        assert_eq!(
            scanout_hal_usage(),
            TextureUses::COLOR_TARGET | TextureUses::COPY_SRC
        );
        assert_eq!(
            scanout_vulkan_usage(),
            vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_SRC
        );
    }

    #[test]
    fn scanout_render_view_is_srgb_over_opaque_unorm_storage() {
        assert_eq!(
            scanout_srgb_view_format(wgpu::TextureFormat::Bgra8Unorm),
            Some(wgpu::TextureFormat::Bgra8UnormSrgb)
        );
        assert_eq!(
            scanout_srgb_view_format(wgpu::TextureFormat::Rgba8Unorm),
            Some(wgpu::TextureFormat::Rgba8UnormSrgb)
        );
        assert_eq!(
            scanout_srgb_vulkan_view_format(vk::Format::B8G8R8A8_UNORM),
            Some(vk::Format::B8G8R8A8_SRGB)
        );
        assert_eq!(
            scanout_srgb_vulkan_view_format(vk::Format::R8G8B8A8_UNORM),
            Some(vk::Format::R8G8B8A8_SRGB)
        );
    }

    #[test]
    fn plain_scanout_contract_srgb_view_renders_and_copies_back_on_headless_gpu() {
        const WIDTH: u32 = 2;
        const HEIGHT: u32 = 2;
        const PADDED_ROW_BYTES: u32 = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;

        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::VULKAN,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        let adapter = match bevy::tasks::block_on(
            instance.request_adapter(&wgpu::RequestAdapterOptions::default()),
        ) {
            Ok(adapter) => adapter,
            Err(error) => {
                eprintln!(
                    "SKIP plain_scanout_contract_srgb_view_renders_and_copies_back_on_headless_gpu: no headless Vulkan adapter: {error}"
                );
                return;
            }
        };
        let required_usage = wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC;
        if !adapter
            .get_texture_format_features(wgpu::TextureFormat::Bgra8Unorm)
            .allowed_usages
            .contains(required_usage)
        {
            eprintln!(
                "SKIP plain_scanout_contract_srgb_view_renders_and_copies_back_on_headless_gpu: adapter lacks BGRA render+copy usage"
            );
            return;
        }
        let (device, queue) = match bevy::tasks::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("CosMix scanout sRGB readback test device"),
                ..Default::default()
            },
        )) {
            Ok(pair) => pair,
            Err(error) => {
                eprintln!(
                    "SKIP plain_scanout_contract_srgb_view_renders_and_copies_back_on_headless_gpu: device request failed: {error}"
                );
                return;
            }
        };

        // A real GBM import requires a DRM device. This plain texture pins the
        // identical wgpu storage/view/usage contract and exercises real GPU
        // render plus COPY_SRC readback on headless builders.
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("CosMix plain scanout-contract readback texture"),
            size: wgpu::Extent3d {
                width: WIDTH,
                height: HEIGHT,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Bgra8Unorm,
            usage: required_usage,
            view_formats: BGRA8_SRGB_VIEW_FORMATS,
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor {
            label: Some("CosMix plain scanout-contract sRGB view"),
            format: Some(wgpu::TextureFormat::Bgra8UnormSrgb),
            ..Default::default()
        });
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("CosMix scanout sRGB readback buffer"),
            size: u64::from(PADDED_ROW_BYTES * HEIGHT),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("CosMix scanout sRGB render and copy encoder"),
        });
        {
            let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("CosMix scanout sRGB clear pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.25,
                            g: 0.5,
                            b: 0.75,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
        }
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(PADDED_ROW_BYTES),
                    rows_per_image: Some(HEIGHT),
                },
            },
            wgpu::Extent3d {
                width: WIDTH,
                height: HEIGHT,
                depth_or_array_layers: 1,
            },
        );
        queue.submit([encoder.finish()]);

        let slice = readback.slice(..);
        let (mapped_sender, mapped_receiver) = mpsc::sync_channel(1);
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = mapped_sender.send(result);
        });
        device
            .poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: Some(Duration::from_secs(5)),
            })
            .expect("headless scanout-contract GPU work completes within 5s");
        mapped_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("headless scanout-contract map callback arrives")
            .expect("headless scanout-contract buffer maps");
        let mapped = slice.get_mapped_range();
        let mut compact = Vec::with_capacity((WIDTH * HEIGHT * 4) as usize);
        for row in 0..HEIGHT as usize {
            let start = row * PADDED_ROW_BYTES as usize;
            compact.extend_from_slice(&mapped[start..start + (WIDTH * 4) as usize]);
        }
        drop(mapped);
        readback.unmap();

        fn checksum(bytes: &[u8]) -> u64 {
            bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
                (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
            })
        }
        let expected_pixel = [225, 188, 137, 255];
        let expected = expected_pixel.repeat((WIDTH * HEIGHT) as usize);
        assert_eq!(
            compact, expected,
            "sRGB view must encode linear clear values"
        );
        assert_eq!(checksum(&compact), checksum(&expected));
    }

    #[test]
    fn scanout_import_rechecks_the_renderer_drm_identity() {
        assert!(validate_renderer_device(226, 226).is_ok());
        assert!(matches!(
            validate_renderer_device(226, 227),
            Err(ScanoutImportError::DeviceMismatch {
                renderer: 226,
                supplied: 227
            })
        ));
    }
}
