use ash::vk;
use drm_fourcc::DrmFourcc;

use crate::DmabufFormat;

pub(crate) fn drm_to_vulkan(fourcc: u32) -> Option<vk::Format> {
    match DrmFourcc::try_from(fourcc).ok()? {
        DrmFourcc::Argb8888 | DrmFourcc::Xrgb8888 => Some(vk::Format::B8G8R8A8_UNORM),
        DrmFourcc::Abgr8888 | DrmFourcc::Xbgr8888 => Some(vk::Format::R8G8B8A8_UNORM),
        _ => None,
    }
}

pub(crate) fn is_opaque(fourcc: u32) -> bool {
    matches!(
        DrmFourcc::try_from(fourcc),
        Ok(DrmFourcc::Xrgb8888 | DrmFourcc::Xbgr8888)
    )
}

pub(crate) fn vulkan_to_wgpu(format: vk::Format) -> Option<wgpu::TextureFormat> {
    match format {
        vk::Format::B8G8R8A8_UNORM => Some(wgpu::TextureFormat::Bgra8Unorm),
        vk::Format::R8G8B8A8_UNORM => Some(wgpu::TextureFormat::Rgba8Unorm),
        _ => None,
    }
}

pub(crate) fn query_importable_formats(
    instance: &ash::Instance,
    physical_device: vk::PhysicalDevice,
) -> Vec<DmabufFormat> {
    // Vulkan WSI exposes each RGBA/BGRA surface format only when linux-dmabuf
    // feedback contains both the alpha and opaque DRM variants. The
    // compositor presents the XRGB/XBGR variants through Bevy's explicit
    // opaque Material2d phase, so their unused byte is never blended.
    let candidates = [
        DrmFourcc::Argb8888,
        DrmFourcc::Xrgb8888,
        DrmFourcc::Abgr8888,
        DrmFourcc::Xbgr8888,
    ];
    let mut formats = Vec::new();

    for fourcc in candidates {
        let Some(vulkan_format) = drm_to_vulkan(fourcc as u32) else {
            continue;
        };
        for modifier in drm_modifier_properties(instance, physical_device, vulkan_format) {
            if modifier.drm_format_modifier_plane_count != 1
                || !modifier
                    .drm_format_modifier_tiling_features
                    .contains(vk::FormatFeatureFlags2::SAMPLED_IMAGE)
                || external_import_properties_with_usage(
                    instance,
                    physical_device,
                    vulkan_format,
                    modifier.drm_format_modifier,
                    vk::ImageUsageFlags::SAMPLED,
                )
                .is_none()
            {
                continue;
            }
            formats.push(DmabufFormat {
                fourcc: fourcc as u32,
                modifier: modifier.drm_format_modifier,
                plane_count: modifier.drm_format_modifier_plane_count,
            });
        }
    }

    formats.sort_unstable_by_key(|format| (format.fourcc, format.modifier));
    formats.dedup();
    formats
}

pub(crate) fn modifier_properties(
    instance: &ash::Instance,
    physical_device: vk::PhysicalDevice,
    format: vk::Format,
    modifier: u64,
) -> Option<vk::DrmFormatModifierProperties2EXT> {
    drm_modifier_properties(instance, physical_device, format)
        .into_iter()
        .find(|properties| properties.drm_format_modifier == modifier)
}

pub(crate) fn drm_modifier_properties(
    instance: &ash::Instance,
    physical_device: vk::PhysicalDevice,
    format: vk::Format,
) -> Vec<vk::DrmFormatModifierProperties2EXT> {
    let mut count = vk::DrmFormatModifierPropertiesList2EXT::default();
    let mut properties = vk::FormatProperties2::default().push_next(&mut count);
    // SAFETY: Both output structures live for the duration of the Vulkan call.
    unsafe {
        instance.get_physical_device_format_properties2(physical_device, format, &mut properties);
    }

    let Ok(length) = usize::try_from(count.drm_format_modifier_count) else {
        return Vec::new();
    };
    let mut values = vec![vk::DrmFormatModifierProperties2EXT::default(); length];
    let returned_count = {
        let mut list = vk::DrmFormatModifierPropertiesList2EXT::default()
            .drm_format_modifier_properties(&mut values);
        let mut properties = vk::FormatProperties2::default().push_next(&mut list);
        // SAFETY: `values` backs the pNext output array and remains alive.
        unsafe {
            instance.get_physical_device_format_properties2(
                physical_device,
                format,
                &mut properties,
            );
        }
        usize::try_from(list.drm_format_modifier_count).unwrap_or_default()
    };
    values.truncate(returned_count.min(values.len()));
    values
}

pub(crate) fn external_import_properties_with_usage(
    instance: &ash::Instance,
    physical_device: vk::PhysicalDevice,
    format: vk::Format,
    modifier: u64,
    usage: vk::ImageUsageFlags,
) -> Option<vk::ImageFormatProperties> {
    external_import_properties_with_usage_and_view_formats(
        instance,
        physical_device,
        format,
        modifier,
        usage,
        &[],
    )
}

pub(crate) fn external_import_properties_with_usage_and_view_formats(
    instance: &ash::Instance,
    physical_device: vk::PhysicalDevice,
    format: vk::Format,
    modifier: u64,
    usage: vk::ImageUsageFlags,
    view_formats: &[vk::Format],
) -> Option<vk::ImageFormatProperties> {
    let mut modifier_info = vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default()
        .drm_format_modifier(modifier)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    let mut external_info = vk::PhysicalDeviceExternalImageFormatInfo::default()
        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let flags = if !view_formats.is_empty() {
        vk::ImageCreateFlags::MUTABLE_FORMAT
    } else {
        Default::default()
    };
    let mut format_list = vk::ImageFormatListCreateInfo::default().view_formats(view_formats);
    let mut format_info = vk::PhysicalDeviceImageFormatInfo2::default()
        .format(format)
        .ty(vk::ImageType::TYPE_2D)
        .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
        .usage(usage)
        .flags(flags)
        .push_next(&mut modifier_info)
        .push_next(&mut external_info);
    if !view_formats.is_empty() {
        format_info = format_info.push_next(&mut format_list);
    }
    let mut external_properties = vk::ExternalImageFormatProperties::default();
    let (result, image_format_properties) = {
        let mut image_properties =
            vk::ImageFormatProperties2::default().push_next(&mut external_properties);
        // SAFETY: All pNext structures remain alive for the query.
        let result = unsafe {
            instance.get_physical_device_image_format_properties2(
                physical_device,
                &format_info,
                &mut image_properties,
            )
        };
        (result, image_properties.image_format_properties)
    };
    (result.is_ok()
        && external_properties
            .external_memory_properties
            .external_memory_features
            .contains(vk::ExternalMemoryFeatureFlags::IMPORTABLE))
    .then_some(image_format_properties)
}

pub(crate) fn main_device_number(
    instance: &ash::Instance,
    physical_device: vk::PhysicalDevice,
) -> Option<u64> {
    let mut drm = vk::PhysicalDeviceDrmPropertiesEXT::default();
    let mut properties = vk::PhysicalDeviceProperties2::default().push_next(&mut drm);
    // SAFETY: `drm` is a valid output structure for this physical-device query.
    unsafe {
        instance.get_physical_device_properties2(physical_device, &mut properties);
    }

    let (major, minor) = if drm.has_render != 0 {
        (drm.render_major, drm.render_minor)
    } else if drm.has_primary != 0 {
        (drm.primary_major, drm.primary_minor)
    } else {
        return None;
    };
    Some(libc::makedev(major.try_into().ok()?, minor.try_into().ok()?) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alpha_and_opaque_desktop_formats_share_vulkan_storage() {
        assert_eq!(
            drm_to_vulkan(DrmFourcc::Argb8888 as u32),
            Some(vk::Format::B8G8R8A8_UNORM)
        );
        assert_eq!(
            drm_to_vulkan(DrmFourcc::Abgr8888 as u32),
            Some(vk::Format::R8G8B8A8_UNORM)
        );
        assert_eq!(
            drm_to_vulkan(DrmFourcc::Xrgb8888 as u32),
            Some(vk::Format::B8G8R8A8_UNORM)
        );
        assert_eq!(
            drm_to_vulkan(DrmFourcc::Xbgr8888 as u32),
            Some(vk::Format::R8G8B8A8_UNORM)
        );
        assert!(is_opaque(DrmFourcc::Xrgb8888 as u32));
        assert!(is_opaque(DrmFourcc::Xbgr8888 as u32));
        assert!(!is_opaque(DrmFourcc::Argb8888 as u32));
        assert!(!is_opaque(DrmFourcc::Abgr8888 as u32));
        assert_eq!(drm_to_vulkan(DrmFourcc::Nv12 as u32), None);
    }
}
