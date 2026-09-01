use std::{
    ffi::{CStr, CString},
    sync::Arc,
};

use ash::vk;
use bevy::{
    app::{Plugin, PluginGroup, PluginGroupBuilder},
    render::{
        RenderDebugFlags, RenderPlugin,
        renderer::{
            RenderAdapter, RenderAdapterInfo, RenderDevice, RenderInstance, RenderQueue,
            WgpuWrapper,
        },
        settings::{RenderCreation, RenderResources},
    },
};
use thiserror::Error;
use wgpu_hal::api::Vulkan;

use crate::{
    DmabufCapabilities, DmabufValidator, VulkanDrmProbe, WgpuWaitForSubmittedWork,
    drm::{describe_physical_device, selected_adapter, selected_adapter_index},
    formats::{main_device_number, query_importable_formats},
};

const VULKAN_API_VERSION: u32 = vk::API_VERSION_1_2;

#[derive(Debug, Error)]
pub enum RendererInitError {
    #[error("failed to load Vulkan: {0}")]
    Load(#[from] ash::LoadingError),
    #[error("Vulkan call failed: {0}")]
    Vulkan(#[from] vk::Result),
    #[error("{0}")]
    Message(String),
}

/// Manually-created Vulkan/wgpu resources plus the exact DMA-BUF feedback set.
pub struct ManualVulkanRenderer {
    resources: RenderResources,
    capabilities: DmabufCapabilities,
    drm_device: Option<u64>,
}

/// The only capability the probe-relaxed construction exposes. Holding the
/// full [`ManualVulkanRenderer`] here would let a caller construct with an
/// empty sampled-import set and then use it as a normal/live renderer,
/// bypassing the invariant the other constructors enforce.
pub struct ScanoutProbeRenderer {
    renderer: ManualVulkanRenderer,
}

impl ScanoutProbeRenderer {
    pub fn scanout_import_capabilities(&self) -> crate::ScanoutImportCapabilities {
        self.renderer.scanout_import_capabilities()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SampledImportRequirement {
    Required,
    NotRequiredForScanoutProbe,
}

impl SampledImportRequirement {
    fn admits(self, sampled_formats_empty: bool) -> bool {
        !sampled_formats_empty || self == Self::NotRequiredForScanoutProbe
    }
}

impl ManualVulkanRenderer {
    pub fn new() -> Result<Self, RendererInitError> {
        Self::new_for_target(None, SampledImportRequirement::Required)
    }

    /// Create an offscreen renderer on the Vulkan physical device whose DRM
    /// primary or render `dev_t` matches `drm_device`. The complete extension
    /// set returned by wgpu-hal's `desired_extensions()` remains enabled to
    /// honour `Instance::from_raw`'s safety contract; unused display
    /// extensions are harmless and are not required by this renderer.
    pub fn new_for_drm_offscreen(drm_device: u64) -> Result<Self, RendererInitError> {
        Self::new_for_target(Some(drm_device), SampledImportRequirement::Required)
    }

    /// Construct only the Vulkan/wgpu handles needed by read-only atomic
    /// scanout admission. Unlike normal renderer construction, this does not
    /// require a non-empty sampled-image DMA-BUF client-import set: scanout
    /// colour attachments are a separate external-memory contract.
    ///
    /// Returns a NARROW handle on purpose: the relaxed construction must not
    /// be usable as a live renderer, or a caller could bypass the
    /// sampled-import invariant the other constructors enforce.
    pub fn new_for_drm_scanout_probe(
        drm_device: u64,
    ) -> Result<ScanoutProbeRenderer, RendererInitError> {
        Ok(ScanoutProbeRenderer {
            renderer: Self::new_for_target(
                Some(drm_device),
                SampledImportRequirement::NotRequiredForScanoutProbe,
            )?,
        })
    }

    /// Read-only Vulkan discovery. This creates no logical device or surface
    /// and never opens the named DRM node.
    pub fn probe_drm(drm_device: Option<u64>) -> Result<VulkanDrmProbe, RendererInitError> {
        let entry = unsafe { ash::Entry::load()? };
        let supported_version =
            unsafe { entry.try_enumerate_instance_version()? }.unwrap_or(vk::API_VERSION_1_0);
        if supported_version < VULKAN_API_VERSION {
            return Err(RendererInitError::Message(format!(
                "Vulkan 1.2 is required, loader reports {}.{}.{}",
                vk::api_version_major(supported_version),
                vk::api_version_minor(supported_version),
                vk::api_version_patch(supported_version)
            )));
        }

        let flags = wgpu::InstanceFlags::default().with_env();
        let mut instance_extensions = <Vulkan as wgpu_hal::Api>::Instance::desired_extensions(
            &entry,
            VULKAN_API_VERSION,
            flags,
        )
        .map_err(|error| RendererInitError::Message(error.to_string()))?;
        deduplicate_extensions(&mut instance_extensions);
        let enabled_instance_extensions = instance_extensions
            .iter()
            .map(|extension| extension.to_string_lossy().into_owned())
            .collect();
        let extension_pointers = instance_extensions
            .iter()
            .map(|extension| extension.as_ptr())
            .collect::<Vec<_>>();
        let application_name =
            CString::new("cosmix-comp").expect("static application name has no NUL");
        let engine_name = CString::new("bevy").expect("static engine name has no NUL");
        let application_info = vk::ApplicationInfo::default()
            .application_name(&application_name)
            .application_version(1)
            .engine_name(&engine_name)
            .engine_version(19)
            .api_version(VULKAN_API_VERSION);
        let raw_instance = unsafe {
            entry.create_instance(
                &vk::InstanceCreateInfo::default()
                    .application_info(&application_info)
                    .enabled_extension_names(&extension_pointers),
                None,
            )?
        };
        let inspected = (|| {
            let mut physical_devices = unsafe { raw_instance.enumerate_physical_devices()? };
            physical_devices.sort_by_key(|device| {
                let properties = unsafe { raw_instance.get_physical_device_properties(*device) };
                device_preference(properties.device_type)
            });
            let adapters = physical_devices
                .into_iter()
                .map(|physical_device| describe_physical_device(&raw_instance, physical_device))
                .collect::<Result<Vec<_>, _>>()?;
            let selected_adapter =
                drm_device.and_then(|target| selected_adapter(&adapters, target));
            Ok(VulkanDrmProbe {
                enabled_instance_extensions,
                adapters,
                selected_adapter,
            })
        })();
        unsafe {
            raw_instance.destroy_instance(None);
        }
        inspected
    }

    fn new_for_target(
        drm_device: Option<u64>,
        sampled_imports: SampledImportRequirement,
    ) -> Result<Self, RendererInitError> {
        let entry = unsafe { ash::Entry::load()? };
        let supported_version =
            unsafe { entry.try_enumerate_instance_version()? }.unwrap_or(vk::API_VERSION_1_0);
        if supported_version < VULKAN_API_VERSION {
            return Err(RendererInitError::Message(format!(
                "Vulkan 1.2 is required, loader reports {}.{}.{}",
                vk::api_version_major(supported_version),
                vk::api_version_minor(supported_version),
                vk::api_version_patch(supported_version)
            )));
        }

        let flags = wgpu::InstanceFlags::default().with_env();
        let mut instance_extensions = <Vulkan as wgpu_hal::Api>::Instance::desired_extensions(
            &entry,
            VULKAN_API_VERSION,
            flags,
        )
        .map_err(|error| RendererInitError::Message(error.to_string()))?;
        deduplicate_extensions(&mut instance_extensions);
        let extension_pointers = instance_extensions
            .iter()
            .map(|extension| extension.as_ptr())
            .collect::<Vec<_>>();
        let application_name =
            CString::new("cosmix-comp").expect("static application name has no NUL");
        let engine_name = CString::new("bevy").expect("static engine name has no NUL");
        let application_info = vk::ApplicationInfo::default()
            .application_name(&application_name)
            .application_version(1)
            .engine_name(&engine_name)
            .engine_version(19)
            .api_version(VULKAN_API_VERSION);
        let raw_instance = unsafe {
            entry.create_instance(
                &vk::InstanceCreateInfo::default()
                    .application_info(&application_info)
                    .enabled_extension_names(&extension_pointers),
                None,
            )?
        };

        let layers = unsafe { entry.enumerate_instance_layer_properties()? };
        let has_nv_optimus = layers.iter().any(|layer| {
            layer
                .layer_name_as_c_str()
                .is_ok_and(|name| name == c"VK_LAYER_NV_optimus")
        });
        let hal_instance = unsafe {
            <Vulkan as wgpu_hal::Api>::Instance::from_raw(
                entry,
                raw_instance.clone(),
                VULKAN_API_VERSION,
                0,
                None,
                instance_extensions,
                flags,
                wgpu::MemoryBudgetThresholds::default(),
                has_nv_optimus,
                None,
            )
            .map_err(|error| RendererInitError::Message(error.to_string()))?
        };

        let required_external_extensions = [
            ash::ext::image_drm_format_modifier::NAME,
            ash::ext::external_memory_dma_buf::NAME,
            ash::ext::queue_family_foreign::NAME,
            ash::khr::external_memory_fd::NAME,
            ash::ext::physical_device_drm::NAME,
        ];
        let mut physical_devices = unsafe { raw_instance.enumerate_physical_devices()? };
        physical_devices.sort_by_key(|device| {
            let properties = unsafe { raw_instance.get_physical_device_properties(*device) };
            device_preference(properties.device_type)
        });
        if let Some(drm_device) = drm_device {
            let adapters = physical_devices
                .iter()
                .map(|physical_device| describe_physical_device(&raw_instance, *physical_device))
                .collect::<Result<Vec<_>, _>>()?;
            let selected_index =
                selected_adapter_index(&adapters, drm_device).ok_or_else(|| {
                    RendererInitError::Message(format!(
                        "no Vulkan physical device has DRM primary/render dev_t {drm_device}"
                    ))
                })?;
            physical_devices = vec![physical_devices[selected_index]];
        }

        let mut selected = None;
        for physical_device in physical_devices {
            let available =
                unsafe { raw_instance.enumerate_device_extension_properties(physical_device)? };
            let has_required = required_external_extensions.iter().all(|required| {
                available
                    .iter()
                    .any(|extension| extension.extension_name_as_c_str() == Ok(*required))
            });
            if !has_required {
                continue;
            }
            let Some(exposed) = hal_instance.expose_adapter(physical_device) else {
                continue;
            };
            let drm_adapter = describe_physical_device(&raw_instance, physical_device)?;
            let formats = query_importable_formats(&raw_instance, physical_device);
            let Some(main_device) = main_device_number(&raw_instance, physical_device) else {
                continue;
            };
            if !sampled_imports.admits(formats.is_empty()) {
                continue;
            }
            selected = Some((physical_device, exposed, drm_adapter, main_device, formats));
            break;
        }
        let (physical_device, exposed, drm_adapter, main_device, formats) =
            selected.ok_or_else(|| {
                RendererInitError::Message(match drm_device {
                    Some(device) => format!(
                        "no Vulkan adapter matching DRM dev_t {device} supports DMA-BUF external-memory RGB import"
                    ),
                    None => "no Vulkan adapter supports DMA-BUF external-memory RGB import".into(),
                })
            })?;

        let mut features = exposed.features;
        if exposed.info.device_type == wgpu::DeviceType::DiscreteGpu {
            features.remove(wgpu::Features::MAPPABLE_PRIMARY_BUFFERS);
        }
        let limits = exposed.capabilities.limits.clone();
        let memory_hints = wgpu::MemoryHints::Performance;
        let mut device_extensions = exposed.adapter.required_device_extensions(features);
        device_extensions.extend(required_external_extensions);
        deduplicate_extensions(&mut device_extensions);
        let extension_pointers = device_extensions
            .iter()
            .map(|extension| extension.as_ptr())
            .collect::<Vec<_>>();
        let mut device_features = exposed
            .adapter
            .physical_device_features(&device_extensions, features);
        let queue_priorities = [1.0];
        let queue_info = vk::DeviceQueueCreateInfo::default()
            .queue_family_index(0)
            .queue_priorities(&queue_priorities);
        let queue_infos = [queue_info];
        let device_info = device_features
            .add_to_device_create(vk::DeviceCreateInfo::default().queue_create_infos(&queue_infos))
            .enabled_extension_names(&extension_pointers);
        let raw_device =
            unsafe { raw_instance.create_device(physical_device, &device_info, None)? };
        let open_device = unsafe {
            exposed
                .adapter
                .device_from_raw(
                    raw_device,
                    None,
                    &device_extensions,
                    features,
                    &limits,
                    &memory_hints,
                    0,
                    0,
                )
                .map_err(|error| RendererInitError::Message(error.to_string()))?
        };

        let instance = unsafe { wgpu::Instance::from_hal::<Vulkan>(hal_instance) };
        let adapter = unsafe { instance.create_adapter_from_hal(exposed) };
        let adapter_info = adapter.get_info();
        let descriptor = wgpu::DeviceDescriptor {
            label: Some("cosmix-comp Vulkan device"),
            required_features: features,
            required_limits: limits,
            experimental_features: unsafe { wgpu::ExperimentalFeatures::enabled() },
            memory_hints,
            trace: wgpu::Trace::Off,
        };
        let (device, queue) = unsafe {
            adapter
                .create_device_from_hal(open_device, &descriptor)
                .map_err(|error| RendererInitError::Message(error.to_string()))?
        };
        let capabilities = DmabufCapabilities {
            main_device,
            formats,
            adapter_name: adapter_info.name.clone(),
            drm_adapter,
        };
        let resources = RenderResources(
            RenderDevice::from(device),
            RenderQueue(Arc::new(WgpuWrapper::new(queue))),
            RenderAdapterInfo(WgpuWrapper::new(adapter_info)),
            RenderAdapter(Arc::new(WgpuWrapper::new(adapter))),
            RenderInstance(Arc::new(WgpuWrapper::new(instance))),
        );

        Ok(Self {
            resources,
            capabilities,
            drm_device,
        })
    }

    pub fn capabilities(&self) -> &DmabufCapabilities {
        &self.capabilities
    }

    pub fn dmabuf_validator(&self) -> DmabufValidator {
        DmabufValidator::new(self.resources.0.clone())
    }

    /// Clone the renderer device handle used by the explicit-sync retirement
    /// worker to wait for already-submitted GPU work.
    pub fn retirement_adapter(&self) -> WgpuWaitForSubmittedWork {
        WgpuWaitForSubmittedWork::with_queue(self.resources.0.clone(), self.resources.1.clone())
    }

    /// Clone the read-only capability-query handles used to negotiate atomic
    /// scanout formats. This creates no image and submits no GPU work.
    pub fn scanout_import_capabilities(&self) -> crate::ScanoutImportCapabilities {
        crate::ScanoutImportCapabilities::new(self.resources.4.clone(), self.resources.3.clone())
    }

    /// Clone the renderer handles needed to import compositor-owned GBM
    /// scanout targets. Nested renderers have no pinned DRM identity and
    /// therefore cannot manufacture this live-only bridge.
    pub fn scanout_render_bridge(&self) -> Option<crate::ScanoutRenderBridge> {
        Some(crate::ScanoutRenderBridge::new(
            self.resources.4.clone(),
            self.resources.3.clone(),
            self.resources.0.clone(),
            self.resources.1.clone(),
            self.drm_device?,
        ))
    }

    /// Clone the renderer handles used for client-owned screencopy destination
    /// imports. `main_device` keeps feedback and advertisement identity
    /// consistent; a submitted buffer does not expose its allocation identity,
    /// so individual capture imports cannot validate it.
    pub fn capture_destination_bridge(&self) -> crate::CaptureDestinationBridge {
        crate::CaptureDestinationBridge::new(
            self.resources.4.clone(),
            self.resources.3.clone(),
            self.resources.0.clone(),
            self.resources.1.clone(),
            self.capabilities.main_device,
        )
    }

    /// Replace Bevy's automatic `RenderPlugin` in its original plugin-group slot.
    pub fn install_into<G: PluginGroup>(self, plugins: G) -> PluginGroupBuilder {
        plugins
            .build()
            .disable::<RenderPlugin>()
            .add_before::<RenderPlugin>(ManualVulkanPlugin {
                resources: self.resources,
            })
    }
}

fn device_preference(device_type: vk::PhysicalDeviceType) -> u8 {
    match device_type {
        vk::PhysicalDeviceType::DISCRETE_GPU => 0,
        vk::PhysicalDeviceType::INTEGRATED_GPU => 1,
        vk::PhysicalDeviceType::VIRTUAL_GPU => 2,
        vk::PhysicalDeviceType::OTHER => 3,
        vk::PhysicalDeviceType::CPU => 4,
        _ => 5,
    }
}

struct ManualVulkanPlugin {
    resources: RenderResources,
}

impl Plugin for ManualVulkanPlugin {
    fn build(&self, app: &mut bevy::prelude::App) {
        app.add_plugins(RenderPlugin {
            render_creation: RenderCreation::Manual(self.resources.clone()),
            synchronous_pipeline_compilation: false,
            debug_flags: RenderDebugFlags::default(),
        });
    }
}

fn deduplicate_extensions(extensions: &mut Vec<&'static CStr>) {
    let mut unique = Vec::with_capacity(extensions.len());
    for extension in extensions.drain(..) {
        if !unique.contains(&extension) {
            unique.push(extension);
        }
    }
    *extensions = unique;
}

#[cfg(test)]
mod tests {
    use super::SampledImportRequirement;

    #[test]
    fn scanout_probe_allows_an_empty_sampled_set_without_weakening_renderer_policy() {
        assert!(!SampledImportRequirement::Required.admits(true));
        assert!(SampledImportRequirement::Required.admits(false));
        assert!(SampledImportRequirement::NotRequiredForScanoutProbe.admits(true));
    }
}
