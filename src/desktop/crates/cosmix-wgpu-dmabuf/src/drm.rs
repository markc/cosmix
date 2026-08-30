use ash::vk;

use crate::init::RendererInitError;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VulkanDrmAdapter {
    pub name: String,
    pub device_type: String,
    pub primary_device: Option<u64>,
    pub render_device: Option<u64>,
}

impl VulkanDrmAdapter {
    fn matches(&self, device: u64) -> bool {
        self.primary_device == Some(device) || self.render_device == Some(device)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VulkanDrmProbe {
    pub enabled_instance_extensions: Vec<String>,
    pub adapters: Vec<VulkanDrmAdapter>,
    pub selected_adapter: Option<VulkanDrmAdapter>,
}

pub(crate) fn describe_physical_device(
    instance: &ash::Instance,
    physical_device: vk::PhysicalDevice,
) -> Result<VulkanDrmAdapter, RendererInitError> {
    let properties = unsafe { instance.get_physical_device_properties(physical_device) };
    let name = properties
        .device_name_as_c_str()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "<invalid Vulkan device name>".into());
    let available = unsafe { instance.enumerate_device_extension_properties(physical_device)? };
    let has_drm_properties = available.iter().any(|extension| {
        extension.extension_name_as_c_str() == Ok(ash::ext::physical_device_drm::NAME)
    });
    if !has_drm_properties {
        return Ok(VulkanDrmAdapter {
            name,
            device_type: format!("{:?}", properties.device_type),
            primary_device: None,
            render_device: None,
        });
    }

    let mut drm = vk::PhysicalDeviceDrmPropertiesEXT::default();
    let mut properties2 = vk::PhysicalDeviceProperties2::default().push_next(&mut drm);
    unsafe {
        instance.get_physical_device_properties2(physical_device, &mut properties2);
    }

    Ok(VulkanDrmAdapter {
        name,
        device_type: format!("{:?}", properties.device_type),
        primary_device: if drm.has_primary != 0 {
            Some(make_device_id(drm.primary_major, drm.primary_minor)?)
        } else {
            None
        },
        render_device: if drm.has_render != 0 {
            Some(make_device_id(drm.render_major, drm.render_minor)?)
        } else {
            None
        },
    })
}

fn make_device_id(major: i64, minor: i64) -> Result<u64, RendererInitError> {
    let major = u32::try_from(major).map_err(|_| {
        RendererInitError::Message(format!("Vulkan reported invalid DRM major number {major}"))
    })?;
    let minor = u32::try_from(minor).map_err(|_| {
        RendererInitError::Message(format!("Vulkan reported invalid DRM minor number {minor}"))
    })?;
    Ok(libc::makedev(major, minor))
}

pub(crate) fn selected_adapter(
    adapters: &[VulkanDrmAdapter],
    target: u64,
) -> Option<VulkanDrmAdapter> {
    selected_adapter_index(adapters, target).map(|index| adapters[index].clone())
}

pub(crate) fn selected_adapter_index(adapters: &[VulkanDrmAdapter], target: u64) -> Option<usize> {
    adapters.iter().position(|adapter| adapter.matches(target))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adapter(
        name: &str,
        device_type: &str,
        primary_device: u64,
        render_device: u64,
    ) -> VulkanDrmAdapter {
        VulkanDrmAdapter {
            name: name.into(),
            device_type: device_type.into(),
            primary_device: Some(primary_device),
            render_device: Some(render_device),
        }
    }

    #[test]
    fn drm_identity_overrides_discrete_first_in_a_hybrid_table() {
        let adapters = [
            adapter("Discrete", "DISCRETE_GPU", 100, 101),
            adapter("Integrated", "INTEGRATED_GPU", 200, 201),
        ];

        assert_eq!(
            selected_adapter(&adapters, 200).map(|adapter| adapter.name),
            Some("Integrated".into())
        );
        assert_eq!(
            selected_adapter(&adapters, 101).map(|adapter| adapter.name),
            Some("Discrete".into())
        );
        assert!(selected_adapter(&adapters, 999).is_none());
    }

    #[test]
    fn primary_and_render_nodes_both_identify_the_same_adapter() {
        let adapters = [adapter("Integrated", "INTEGRATED_GPU", 200, 201)];

        assert_eq!(
            selected_adapter(&adapters, 200),
            selected_adapter(&adapters, 201)
        );
    }
}
