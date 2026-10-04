//! Vulkan DMA-buf interop. Never touch imported textures outside an acquire/submit/release bracket.
use std::os::fd::AsRawFd;

use anyhow::{ensure, Context as _};
use ash::vk;
use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::allocator::format::FormatSet;
use smithay::backend::allocator::{Buffer, Format, Fourcc, Modifier};
use smithay::backend::drm::DrmNode;

use super::renderer::{VelloError, VelloTexture};

pub(super) const FORMATS: &[(Fourcc, wgpu::TextureFormat)] = &[
    (Fourcc::Argb8888, wgpu::TextureFormat::Bgra8Unorm),
    (Fourcc::Xrgb8888, wgpu::TextureFormat::Bgra8Unorm),
    (Fourcc::Abgr8888, wgpu::TextureFormat::Rgba8Unorm),
    (Fourcc::Xbgr8888, wgpu::TextureFormat::Rgba8Unorm),
    (Fourcc::Abgr2101010, wgpu::TextureFormat::Rgb10a2Unorm),
    (Fourcc::Xbgr2101010, wgpu::TextureFormat::Rgb10a2Unorm),
];
pub(super) fn texture_format(format: Fourcc) -> anyhow::Result<wgpu::TextureFormat> {
    FORMATS
        .iter()
        .find_map(|(f, t)| (*f == format).then_some(*t))
        .context("unsupported Vello pixel format")
}

pub(super) fn identity(dmabuf: &Dmabuf) -> anyhow::Result<(u64, u64)> {
    let fd = dmabuf.handles().next().context("missing DMA-buf fd")?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(fd.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let stat = unsafe { stat.assume_init() };
    Ok((stat.st_dev as u64, stat.st_ino as u64))
}

pub(super) fn matches_node(adapter: &wgpu::Adapter, node: DrmNode) -> bool {
    let Some(hal) = (unsafe { adapter.as_hal::<wgpu::hal::api::Vulkan>() }) else {
        return false;
    };
    let raw = hal.shared_instance().raw_instance();
    let mut drm = vk::PhysicalDeviceDrmPropertiesEXT::default();
    let mut props = vk::PhysicalDeviceProperties2::default().push_next(&mut drm);
    unsafe { raw.get_physical_device_properties2(hal.raw_physical_device(), &mut props) };
    (drm.has_primary != 0
        && libc::makedev(drm.primary_major as _, drm.primary_minor as _) == node.dev_id())
        || (drm.has_render != 0
            && libc::makedev(drm.render_major as _, drm.render_minor as _) == node.dev_id())
}

/// Probe actual modifier image support, including external-memory importability and usage.
pub(super) fn formats(adapter: &wgpu::Adapter, render: bool) -> FormatSet {
    let Some(hal) = (unsafe { adapter.as_hal::<wgpu::hal::api::Vulkan>() }) else {
        return FormatSet::default();
    };
    let raw = hal.shared_instance().raw_instance();
    let phd = hal.raw_physical_device();
    let mut result = Vec::new();
    for &(code, format) in FORMATS {
        let vk_format = hal.texture_format_as_raw(format);
        let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
        let mut props = vk::FormatProperties2::default().push_next(&mut list);
        unsafe { raw.get_physical_device_format_properties2(phd, vk_format, &mut props) };
        let mut modifiers = vec![
            vk::DrmFormatModifierPropertiesEXT::default();
            list.drm_format_modifier_count as usize
        ];
        list.p_drm_format_modifier_properties = modifiers.as_mut_ptr();
        let mut props = vk::FormatProperties2::default().push_next(&mut list);
        unsafe { raw.get_physical_device_format_properties2(phd, vk_format, &mut props) };
        for modifier in modifiers {
            if modifier.drm_format_modifier_plane_count != 1 {
                continue;
            }
            let mut features =
                vk::FormatFeatureFlags::SAMPLED_IMAGE | vk::FormatFeatureFlags::TRANSFER_SRC;
            let mut usage = vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_SRC;
            if render {
                features |= vk::FormatFeatureFlags::COLOR_ATTACHMENT;
                usage |= vk::ImageUsageFlags::COLOR_ATTACHMENT;
            }
            if !modifier
                .drm_format_modifier_tiling_features
                .contains(features)
            {
                continue;
            }
            let mut modifier_info = vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default()
                .drm_format_modifier(modifier.drm_format_modifier)
                .sharing_mode(vk::SharingMode::EXCLUSIVE);
            let mut external = vk::PhysicalDeviceExternalImageFormatInfo::default()
                .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
            let info = vk::PhysicalDeviceImageFormatInfo2::default()
                .format(vk_format)
                .ty(vk::ImageType::TYPE_2D)
                .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
                .usage(usage)
                .push_next(&mut modifier_info)
                .push_next(&mut external);
            let mut external_props = vk::ExternalImageFormatProperties::default();
            let mut props = vk::ImageFormatProperties2::default().push_next(&mut external_props);
            if unsafe { raw.get_physical_device_image_format_properties2(phd, &info, &mut props) }
                .is_err()
            {
                continue;
            }
            if external_props
                .external_memory_properties
                .external_memory_features
                .contains(vk::ExternalMemoryFeatureFlags::IMPORTABLE)
            {
                result.push(Format {
                    code,
                    modifier: modifier.drm_format_modifier.into(),
                });
            }
        }
    }
    result.into_iter().collect()
}

pub(super) fn import(
    device: &wgpu::Device,
    dmabuf: &Dmabuf,
    render: bool,
) -> anyhow::Result<wgpu::Texture> {
    ensure!(
        dmabuf.num_planes() == 1,
        "only single-memory-plane DMA-bufs are supported"
    );
    let format = dmabuf.format();
    ensure!(
        format.modifier != Modifier::Invalid,
        "implicit DMA-buf modifiers are unsupported"
    );
    let mut usage = wgpu::TextureUses::RESOURCE | wgpu::TextureUses::COPY_SRC;
    let mut safe_usage = wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_SRC;
    let sentinel = if render {
        wgpu::TextureUses::COLOR_TARGET
    } else {
        wgpu::TextureUses::RESOURCE
    };
    if render {
        usage |= wgpu::TextureUses::COLOR_TARGET;
        safe_usage |= wgpu::TextureUsages::RENDER_ATTACHMENT;
    }
    let desc = wgpu::hal::TextureDescriptor {
        label: Some("niri imported DMA-buf"),
        size: wgpu::Extent3d {
            width: dmabuf.width(),
            height: dmabuf.height(),
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: texture_format(format.code)?,
        usage,
        memory_flags: wgpu::hal::MemoryFlags::empty(),
        view_formats: vec![],
    };
    let hal =
        unsafe { device.as_hal::<wgpu::hal::api::Vulkan>() }.context("not a Vulkan device")?;
    let texture = unsafe {
        hal.texture_from_dmabuf_fd(
            dmabuf
                .handles()
                .next()
                .context("missing DMA-buf fd")?
                .try_clone_to_owned()?,
            &desc,
            format.modifier.into(),
            u64::from(dmabuf.strides().next().unwrap()),
            u64::from(dmabuf.offsets().next().unwrap()),
        )
    }
    .context("importing Vulkan DMA-buf")?;
    // The tracker sentinel describes the layout AFTER our acquire barrier, not the foreign layout.
    // No wgpu operation is allowed on this resource before that acquire. In particular a
    // queue.write_texture would add a pending upload before A, so imported textures never
    // include COPY_DST and ImportMem rejects them. A/Z are intentionally invisible to wgpu;
    // B restores the sentinel before Z so the next frame cannot insert a stale-layout barrier.
    Ok(unsafe {
        device.create_texture_from_hal::<wgpu::hal::api::Vulkan>(
            texture,
            &wgpu::TextureDescriptor {
                label: desc.label,
                size: desc.size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: desc.dimension,
                format: desc.format,
                usage: safe_usage,
                view_formats: &[],
            },
            sentinel,
        )
    })
}

fn wait(dmabuf: &Dmabuf, writable: bool) -> anyhow::Result<()> {
    for fd in dmabuf.handles() {
        let mut poll = libc::pollfd {
            fd: fd.as_raw_fd(),
            events: if writable {
                libc::POLLOUT
            } else {
                libc::POLLIN
            },
            revents: 0,
        };
        loop {
            let result = unsafe { libc::poll(&mut poll, 1, 30_000) };
            if result < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err.into());
            }
            ensure!(result > 0, "DMA-buf acquire timed out");
            ensure!(
                poll.revents & (libc::POLLERR | libc::POLLNVAL | libc::POLLHUP) == 0,
                "DMA-buf acquire failed"
            );
            ensure!(
                poll.revents & poll.events != 0,
                "DMA-buf acquire returned no readiness"
            );
            break;
        }
    }
    Ok(())
}

/// A and Z are raw-only encoders, B is normal wgpu. No resource transitions run after Z.
pub(super) fn bracket(
    device: &wgpu::Device,
    textures: &[VelloTexture],
) -> Result<Option<(wgpu::CommandBuffer, wgpu::CommandBuffer)>, VelloError> {
    let imported: Vec<_> = textures.iter().filter(|t| t.0.imported.is_some()).collect();
    if imported.is_empty() {
        return Ok(None);
    }
    let hal =
        unsafe { device.as_hal::<wgpu::hal::api::Vulkan>() }.context("not a Vulkan device")?;
    let raw = hal.raw_device();
    let family = hal.queue_family_index();
    let mut acquire = Vec::new();
    let mut release = Vec::new();
    for texture in imported {
        let imported = texture.0.imported.as_ref().unwrap();
        wait(&imported.dmabuf, imported.render)?;
        let hal_texture = unsafe { texture.texture().as_hal::<wgpu::hal::api::Vulkan>() }
            .context("not a Vulkan texture")?;
        let image = unsafe { hal_texture.raw_handle() };
        let (layout, access) = if imported.render {
            (
                vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE,
            )
        } else {
            (
                vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE,
            )
        };
        let range = vk::ImageSubresourceRange::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .level_count(1)
            .layer_count(1);
        acquire.push(
            vk::ImageMemoryBarrier::default()
                .image(image)
                .subresource_range(range)
                .old_layout(vk::ImageLayout::GENERAL)
                .new_layout(layout)
                .src_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
                .dst_queue_family_index(family)
                .src_access_mask(vk::AccessFlags::empty())
                .dst_access_mask(access),
        );
        release.push(
            vk::ImageMemoryBarrier::default()
                .image(image)
                .subresource_range(range)
                .old_layout(layout)
                .new_layout(vk::ImageLayout::GENERAL)
                .src_queue_family_index(family)
                .dst_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
                .src_access_mask(access)
                .dst_access_mask(vk::AccessFlags::empty()),
        );
    }
    let mut a = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("DMA-buf acquire"),
    });
    let mut z = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("DMA-buf release"),
    });
    // Separate encoders are mandatory: wgpu 30 forbids mixing raw and tracked recording.
    unsafe {
        a.as_hal_mut::<wgpu::hal::api::Vulkan, _, _>(|encoder| {
            raw.cmd_pipeline_barrier(
                encoder.unwrap().raw_handle(),
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &acquire,
            );
        });
        z.as_hal_mut::<wgpu::hal::api::Vulkan, _, _>(|encoder| {
            raw.cmd_pipeline_barrier(
                encoder.unwrap().raw_handle(),
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &release,
            );
        });
    }
    Ok(Some((a.finish(), z.finish())))
}

pub(super) fn restore(encoder: &mut wgpu::CommandEncoder, textures: &[VelloTexture]) {
    encoder.transition_resources(
        std::iter::empty(),
        textures.iter().filter_map(|texture| {
            texture
                .0
                .imported
                .as_ref()
                .map(|imported| wgpu::TextureTransition {
                    texture: texture.texture(),
                    selector: None,
                    state: if imported.render {
                        wgpu::TextureUses::COLOR_TARGET
                    } else {
                        wgpu::TextureUses::RESOURCE
                    },
                })
        }),
    );
}
