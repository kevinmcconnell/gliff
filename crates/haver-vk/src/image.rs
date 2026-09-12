//! Images and buffers: NV12 video images with per-plane views, dmabuf
//! import (captured frames) and export (the client's output), and host
//! visible buffers for bitstreams and readback.

use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::sync::Arc;

use ash::vk;
use drm_fourcc::DrmFourcc;

use crate::device::Gpu;
use crate::{Error, Result};

pub const NV12: vk::Format = vk::Format::G8_B8R8_2PLANE_420_UNORM;
pub const LINEAR_MODIFIER: u64 = 0;

/// A dmabuf plane description shared with the capture and display sides.
pub struct DmabufPlane<'a> {
    pub fd: BorrowedFd<'a>,
    pub width: u32,
    pub height: u32,
    pub offset: u32,
    pub stride: u32,
    pub fourcc: DrmFourcc,
    pub modifier: u64,
}

/// A device image plus its memory and the views the pipelines use.
pub struct Image {
    gpu: Arc<Gpu>,
    pub(crate) image: vk::Image,
    memory: vk::DeviceMemory,
    pub format: vk::Format,
    pub width: u32,
    pub height: u32,
    pub layers: u32,
    /// Whole-image view (for video ops), one per array layer.
    pub(crate) layer_views: Vec<vk::ImageView>,
    /// Per-plane views of layer 0 (R8 for Y, R8G8 for UV) for compute.
    pub(crate) plane_views: Vec<vk::ImageView>,
    pub(crate) layout: std::cell::Cell<vk::ImageLayout>,
}

/// How an image is used, which decides its usage flags and sharing.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Encoder input: written by the split shader, read by the encoder.
    EncodeSource,
    /// Encoder reference pictures (one array image, one layer per slot).
    EncodeDpb,
    /// Decoder output: written by the decoder, read by the recombine shader.
    DecodeOutput,
    /// Decoder reference pictures.
    DecodeDpb,
    /// Plain NV12 for tests: upload/download only.
    Staging,
}

impl Image {
    /// Create an NV12 image for a video role. `profile` is the video profile
    /// the image will be used with (required by the video extensions).
    pub(crate) fn nv12(gpu: &Arc<Gpu>, role: Role, width: u32, height: u32, layers: u32, profile: Option<&vk::VideoProfileInfoKHR>) -> Result<Self> {
        let (usage, flags) = match role {
            Role::EncodeSource => (
                vk::ImageUsageFlags::VIDEO_ENCODE_SRC_KHR | vk::ImageUsageFlags::STORAGE | vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::TRANSFER_SRC,
                vk::ImageCreateFlags::MUTABLE_FORMAT | vk::ImageCreateFlags::EXTENDED_USAGE,
            ),
            Role::EncodeDpb => (vk::ImageUsageFlags::VIDEO_ENCODE_DPB_KHR, vk::ImageCreateFlags::empty()),
            Role::DecodeOutput => (
                vk::ImageUsageFlags::VIDEO_DECODE_DST_KHR | vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_SRC,
                vk::ImageCreateFlags::MUTABLE_FORMAT | vk::ImageCreateFlags::EXTENDED_USAGE,
            ),
            Role::DecodeDpb => (vk::ImageUsageFlags::VIDEO_DECODE_DPB_KHR, vk::ImageCreateFlags::empty()),
            Role::Staging => (vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::TRANSFER_SRC | vk::ImageUsageFlags::STORAGE | vk::ImageUsageFlags::SAMPLED, vk::ImageCreateFlags::MUTABLE_FORMAT | vk::ImageCreateFlags::EXTENDED_USAGE),
        };
        let families = gpu.families.all();
        let profiles = profile.map(|p| vec![*p]).unwrap_or_default();
        let mut profile_list = vk::VideoProfileListInfoKHR::default().profiles(&profiles);
        let mut info = vk::ImageCreateInfo::default()
            .flags(flags)
            .image_type(vk::ImageType::TYPE_2D)
            .format(NV12)
            .extent(vk::Extent3D { width, height, depth: 1 })
            .mip_levels(1)
            .array_layers(layers)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(usage)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        if families.len() > 1 {
            info = info.sharing_mode(vk::SharingMode::CONCURRENT).queue_family_indices(&families);
        }
        if profile.is_some() {
            info = info.push_next(&mut profile_list);
        }
        // SAFETY: valid create info; memory is bound before any use.
        let (image, memory) = unsafe {
            let image = gpu.device.create_image(&info, None)?;
            let reqs = gpu.device.get_image_memory_requirements(image);
            let memory = gpu.allocate(reqs, vk::MemoryPropertyFlags::DEVICE_LOCAL, None)?;
            gpu.device.bind_image_memory(image, memory, 0)?;
            (image, memory)
        };
        let mut img = Self { gpu: gpu.clone(), image, memory, format: NV12, width, height, layers, layer_views: Vec::new(), plane_views: Vec::new(), layout: vk::ImageLayout::UNDEFINED.into() };
        let compute_usage = vk::ImageUsageFlags::STORAGE | vk::ImageUsageFlags::SAMPLED;
        let video_usage = usage & !compute_usage;
        for layer in 0..layers {
            img.layer_views.push(img.view(NV12, vk::ImageAspectFlags::COLOR, layer, video_usage)?);
        }
        if matches!(role, Role::EncodeSource | Role::DecodeOutput | Role::Staging) {
            let plane_usage = usage & compute_usage;
            img.plane_views.push(img.view(vk::Format::R8_UNORM, vk::ImageAspectFlags::PLANE_0, 0, plane_usage)?);
            img.plane_views.push(img.view(vk::Format::R8G8_UNORM, vk::ImageAspectFlags::PLANE_1, 0, plane_usage)?);
        }
        Ok(img)
    }

    /// Create a BGRA image whose memory is exportable as a linear dmabuf
    /// (the client's display output).
    pub(crate) fn exportable_bgra(gpu: &Arc<Gpu>, width: u32, height: u32) -> Result<Self> {
        let modifiers = [LINEAR_MODIFIER];
        let mut mod_list = vk::ImageDrmFormatModifierListCreateInfoEXT::default().drm_format_modifiers(&modifiers);
        let mut external = vk::ExternalMemoryImageCreateInfo::default().handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let view_formats = [vk::Format::B8G8R8A8_UNORM, vk::Format::R8G8B8A8_UNORM];
        let mut format_list = vk::ImageFormatListCreateInfo::default().view_formats(&view_formats);
        let info = vk::ImageCreateInfo::default()
            .flags(vk::ImageCreateFlags::MUTABLE_FORMAT)
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk::Format::B8G8R8A8_UNORM)
            .extent(vk::Extent3D { width, height, depth: 1 })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(vk::ImageUsageFlags::STORAGE | vk::ImageUsageFlags::TRANSFER_SRC)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut mod_list)
            .push_next(&mut external)
            .push_next(&mut format_list);
        // SAFETY: valid create info; dedicated exportable allocation bound
        // before use.
        let (image, memory) = unsafe {
            let image = gpu.device.create_image(&info, None)?;
            let mut dedicated_reqs = vk::MemoryDedicatedRequirements::default();
            let mut reqs = vk::MemoryRequirements2::default().push_next(&mut dedicated_reqs);
            gpu.device.get_image_memory_requirements2(&vk::ImageMemoryRequirementsInfo2::default().image(image), &mut reqs);
            let mut export = vk::ExportMemoryAllocateInfo::default().handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
            let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
            let mut alloc = vk::MemoryAllocateInfo::default()
                .allocation_size(reqs.memory_requirements.size)
                .memory_type_index(gpu.memory_type(reqs.memory_requirements.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)?)
                .push_next(&mut export)
                .push_next(&mut dedicated);
            let memory = gpu.device.allocate_memory(&mut alloc, None)?;
            gpu.device.bind_image_memory(image, memory, 0)?;
            (image, memory)
        };
        let mut img = Self { gpu: gpu.clone(), image, memory, format: vk::Format::B8G8R8A8_UNORM, width, height, layers: 1, layer_views: Vec::new(), plane_views: Vec::new(), layout: vk::ImageLayout::UNDEFINED.into() };
        // The shader declares `rgba8`, so it stores through an RGBA view of
        // the BGRA memory with the channels swapped (see recombine.comp).
        img.layer_views.push(img.view(vk::Format::R8G8B8A8_UNORM, vk::ImageAspectFlags::COLOR, 0, vk::ImageUsageFlags::STORAGE)?);
        Ok(img)
    }

    /// A plain single-plane, optimally tiled device image.
    pub(crate) fn plain(gpu: &Arc<Gpu>, format: vk::Format, width: u32, height: u32, usage: vk::ImageUsageFlags) -> Result<Self> {
        let info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D { width, height, depth: 1 })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(usage)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        // SAFETY: valid create info; memory is bound before any use.
        let (image, memory) = unsafe {
            let image = gpu.device.create_image(&info, None)?;
            let reqs = gpu.device.get_image_memory_requirements(image);
            let memory = gpu.allocate(reqs, vk::MemoryPropertyFlags::DEVICE_LOCAL, None)?;
            gpu.device.bind_image_memory(image, memory, 0)?;
            (image, memory)
        };
        let mut img = Self { gpu: gpu.clone(), image, memory, format, width, height, layers: 1, layer_views: Vec::new(), plane_views: Vec::new(), layout: vk::ImageLayout::UNDEFINED.into() };
        img.layer_views.push(img.view(format, vk::ImageAspectFlags::COLOR, 0, usage & vk::ImageUsageFlags::SAMPLED)?);
        Ok(img)
    }

    /// A BGRA image filled by a buffer copy, for the upload path.
    pub(crate) fn bgra_upload(gpu: &Arc<Gpu>, width: u32, height: u32) -> Result<Self> {
        Self::plain(gpu, vk::Format::B8G8R8A8_UNORM, width, height, vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST)
    }

    /// Export the (linear, single-plane) image as a dmabuf for another API.
    pub fn export_dmabuf(&self) -> Result<ExportedDmabuf> {
        // SAFETY: the memory was allocated exportable; the fd is owned by us.
        unsafe {
            let info = vk::MemoryGetFdInfoKHR::default().memory(self.memory).handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
            let fd = self.gpu.external_fd.get_memory_fd(&info)?;
            let fd = OwnedFd::from_raw_fd(fd);
            let mut props = vk::ImageDrmFormatModifierPropertiesEXT::default();
            self.gpu.drm_modifier.get_image_drm_format_modifier_properties(self.image, &mut props)?;
            let sub = vk::ImageSubresource::default().aspect_mask(vk::ImageAspectFlags::MEMORY_PLANE_0_EXT);
            let layout = self.gpu.device.get_image_subresource_layout(self.image, sub);
            Ok(ExportedDmabuf {
                fd,
                width: self.width,
                height: self.height,
                fourcc: DrmFourcc::Xrgb8888,
                modifier: props.drm_format_modifier,
                offset: layout.offset as u32,
                stride: layout.row_pitch as u32,
            })
        }
    }

    /// Import a captured dmabuf (single-plane 32-bit RGB) for sampling.
    pub(crate) fn import_dmabuf(gpu: &Arc<Gpu>, plane: &DmabufPlane) -> Result<Self> {
        let format = match plane.fourcc {
            DrmFourcc::Xrgb8888 | DrmFourcc::Argb8888 => vk::Format::B8G8R8A8_UNORM,
            DrmFourcc::Xbgr8888 | DrmFourcc::Abgr8888 => vk::Format::R8G8B8A8_UNORM,
            other => return Err(Error::Unsupported(format!("capture format {other:?}"))),
        };
        let layouts = [vk::SubresourceLayout { offset: plane.offset as u64, size: 0, row_pitch: plane.stride as u64, array_pitch: 0, depth_pitch: 0 }];
        let mut explicit = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default().drm_format_modifier(plane.modifier).plane_layouts(&layouts);
        let mut external = vk::ExternalMemoryImageCreateInfo::default().handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D { width: plane.width, height: plane.height, depth: 1 })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(vk::ImageUsageFlags::SAMPLED)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut explicit)
            .push_next(&mut external);
        // SAFETY: the fd is duplicated for Vulkan, which takes ownership of
        // the duplicate on a successful import.
        let (image, memory) = unsafe {
            let image = gpu.device.create_image(&info, None)?;
            let reqs = gpu.device.get_image_memory_requirements(image);
            let mut fd_props = vk::MemoryFdPropertiesKHR::default();
            gpu.external_fd.get_memory_fd_properties(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT, plane.fd.as_raw_fd(), &mut fd_props)?;
            let dup = libc_dup(plane.fd)?;
            let mut import = vk::ImportMemoryFdInfoKHR::default().handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT).fd(dup.as_raw_fd());
            let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
            let type_bits = reqs.memory_type_bits & fd_props.memory_type_bits;
            let mut alloc = vk::MemoryAllocateInfo::default()
                .allocation_size(reqs.size)
                .memory_type_index(gpu.memory_type(type_bits, vk::MemoryPropertyFlags::empty())?)
                .push_next(&mut import)
                .push_next(&mut dedicated);
            let memory = match gpu.device.allocate_memory(&mut alloc, None) {
                Ok(m) => {
                    std::mem::forget(dup);
                    m
                }
                Err(e) => {
                    gpu.device.destroy_image(image, None);
                    return Err(e.into());
                }
            };
            gpu.device.bind_image_memory(image, memory, 0)?;
            (image, memory)
        };
        let mut img = Self { gpu: gpu.clone(), image, memory, format, width: plane.width, height: plane.height, layers: 1, layer_views: Vec::new(), plane_views: Vec::new(), layout: vk::ImageLayout::UNDEFINED.into() };
        img.layer_views.push(img.view(format, vk::ImageAspectFlags::COLOR, 0, vk::ImageUsageFlags::SAMPLED)?);
        Ok(img)
    }

    /// A view limited to `usage`, which must be a subset of the image's usage:
    /// plane views carry only the compute usages, whole-image views only the
    /// video ones, since the plane formats and NV12 support different features.
    fn view(&self, format: vk::Format, aspect: vk::ImageAspectFlags, layer: u32, usage: vk::ImageUsageFlags) -> Result<vk::ImageView> {
        let mut usage_info = vk::ImageViewUsageCreateInfo::default().usage(usage);
        let info = vk::ImageViewCreateInfo::default()
            .image(self.image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(format)
            .subresource_range(vk::ImageSubresourceRange { aspect_mask: aspect, base_mip_level: 0, level_count: 1, base_array_layer: layer, layer_count: 1 })
            .push_next(&mut usage_info);
        // SAFETY: valid image and range.
        Ok(unsafe { self.gpu.device.create_image_view(&info, None) }?)
    }

    pub(crate) fn view0(&self) -> vk::ImageView {
        self.layer_views[0]
    }

    /// Record a layout transition for all layers, with a full barrier.
    pub(crate) fn transition(&self, cmd: vk::CommandBuffer, new: vk::ImageLayout) {
        let old = self.layout.get();
        if old == new {
            return;
        }
        let barrier = vk::ImageMemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
            .src_access_mask(vk::AccessFlags2::MEMORY_WRITE | vk::AccessFlags2::MEMORY_READ)
            .dst_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
            .dst_access_mask(vk::AccessFlags2::MEMORY_WRITE | vk::AccessFlags2::MEMORY_READ)
            .old_layout(old)
            .new_layout(new)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(self.image)
            .subresource_range(vk::ImageSubresourceRange { aspect_mask: vk::ImageAspectFlags::COLOR, base_mip_level: 0, level_count: 1, base_array_layer: 0, layer_count: self.layers });
        let barriers = [barrier];
        let dep = vk::DependencyInfo::default().image_memory_barriers(&barriers);
        // SAFETY: recording into a command buffer in the recording state.
        unsafe { self.gpu.device.cmd_pipeline_barrier2(cmd, &dep) };
        self.layout.set(new);
    }

    /// Full memory barrier without a layout change (between a write and a
    /// read of the same image in the same layout).
    pub(crate) fn memory_barrier(&self, cmd: vk::CommandBuffer) {
        let barrier = vk::MemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
            .src_access_mask(vk::AccessFlags2::MEMORY_WRITE)
            .dst_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
            .dst_access_mask(vk::AccessFlags2::MEMORY_READ | vk::AccessFlags2::MEMORY_WRITE);
        let barriers = [barrier];
        let dep = vk::DependencyInfo::default().memory_barriers(&barriers);
        // SAFETY: recording into a command buffer in the recording state.
        unsafe { self.gpu.device.cmd_pipeline_barrier2(cmd, &dep) };
    }

    /// Record a copy of the visible NV12 planes into `buf` (Y then UV,
    /// tightly packed at `width`).
    pub(crate) fn copy_nv12_to_buffer(&self, cmd: vk::CommandBuffer, layer: u32, buf: &HostBuffer, width: u32, height: u32) {
        let regions = [
            vk::BufferImageCopy { buffer_offset: 0, buffer_row_length: width, buffer_image_height: height, image_subresource: layers(vk::ImageAspectFlags::PLANE_0, layer), image_offset: vk::Offset3D::default(), image_extent: vk::Extent3D { width, height, depth: 1 } },
            vk::BufferImageCopy { buffer_offset: (width * height) as u64, buffer_row_length: width / 2, buffer_image_height: height / 2, image_subresource: layers(vk::ImageAspectFlags::PLANE_1, layer), image_offset: vk::Offset3D::default(), image_extent: vk::Extent3D { width: width / 2, height: height / 2, depth: 1 } },
        ];
        // SAFETY: valid regions within both the image and the buffer.
        unsafe { self.gpu.device.cmd_copy_image_to_buffer(cmd, self.image, self.layout.get(), buf.buffer, &regions) };
    }

    /// Record a copy of packed NV12 planes from `buf` into the image.
    pub(crate) fn copy_nv12_from_buffer(&self, cmd: vk::CommandBuffer, buf: &HostBuffer, width: u32, height: u32) {
        let regions = [
            vk::BufferImageCopy { buffer_offset: 0, buffer_row_length: width, buffer_image_height: height, image_subresource: layers(vk::ImageAspectFlags::PLANE_0, 0), image_offset: vk::Offset3D::default(), image_extent: vk::Extent3D { width, height, depth: 1 } },
            vk::BufferImageCopy { buffer_offset: (width * height) as u64, buffer_row_length: width / 2, buffer_image_height: height / 2, image_subresource: layers(vk::ImageAspectFlags::PLANE_1, 0), image_offset: vk::Offset3D::default(), image_extent: vk::Extent3D { width: width / 2, height: height / 2, depth: 1 } },
        ];
        // SAFETY: valid regions within both the image and the buffer.
        unsafe { self.gpu.device.cmd_copy_buffer_to_image(cmd, buf.buffer, self.image, self.layout.get(), &regions) };
    }

    /// Record a copy of packed 32-bit pixels from `buf` into the image.
    pub(crate) fn copy_rgba_from_buffer(&self, cmd: vk::CommandBuffer, buf: &HostBuffer) {
        let regions = [vk::BufferImageCopy { buffer_offset: 0, buffer_row_length: self.width, buffer_image_height: self.height, image_subresource: layers(vk::ImageAspectFlags::COLOR, 0), image_offset: vk::Offset3D::default(), image_extent: vk::Extent3D { width: self.width, height: self.height, depth: 1 } }];
        // SAFETY: valid regions within both the image and the buffer.
        unsafe { self.gpu.device.cmd_copy_buffer_to_image(cmd, buf.buffer, self.image, self.layout.get(), &regions) };
    }

    /// Record a copy of a single-plane 32-bit image into `buf`, packed.
    pub(crate) fn copy_rgba_to_buffer(&self, cmd: vk::CommandBuffer, buf: &HostBuffer) {
        let regions = [vk::BufferImageCopy { buffer_offset: 0, buffer_row_length: self.width, buffer_image_height: self.height, image_subresource: layers(vk::ImageAspectFlags::COLOR, 0), image_offset: vk::Offset3D::default(), image_extent: vk::Extent3D { width: self.width, height: self.height, depth: 1 } }];
        // SAFETY: valid regions within both the image and the buffer.
        unsafe { self.gpu.device.cmd_copy_image_to_buffer(cmd, self.image, self.layout.get(), buf.buffer, &regions) };
    }
}

fn layers(aspect: vk::ImageAspectFlags, layer: u32) -> vk::ImageSubresourceLayers {
    vk::ImageSubresourceLayers { aspect_mask: aspect, mip_level: 0, base_array_layer: layer, layer_count: 1 }
}

impl Drop for Image {
    fn drop(&mut self) {
        // SAFETY: owners wait for GPU work that uses the image before dropping.
        unsafe {
            for v in self.plane_views.drain(..).chain(self.layer_views.drain(..)) {
                self.gpu.device.destroy_image_view(v, None);
            }
            self.gpu.device.destroy_image(self.image, None);
            self.gpu.device.free_memory(self.memory, None);
        }
    }
}

/// A dmabuf handed to another API (GTK), describing one linear plane.
#[derive(Debug)]
pub struct ExportedDmabuf {
    pub fd: OwnedFd,
    pub width: u32,
    pub height: u32,
    pub fourcc: DrmFourcc,
    pub modifier: u64,
    pub offset: u32,
    pub stride: u32,
}

fn libc_dup(fd: BorrowedFd) -> Result<OwnedFd> {
    Ok(fd.try_clone_to_owned()?)
}

/// A host-visible, persistently mapped buffer.
pub struct HostBuffer {
    gpu: Arc<Gpu>,
    pub(crate) buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    ptr: *mut u8,
    pub size: usize,
}

impl HostBuffer {
    pub(crate) fn new(gpu: &Arc<Gpu>, size: usize, usage: vk::BufferUsageFlags, profile: Option<&vk::VideoProfileInfoKHR>) -> Result<Self> {
        let profiles = profile.map(|p| vec![*p]).unwrap_or_default();
        let mut list = vk::VideoProfileListInfoKHR::default().profiles(&profiles);
        let mut info = vk::BufferCreateInfo::default().size(size as u64).usage(usage);
        if profile.is_some() {
            info = info.push_next(&mut list);
        }
        // SAFETY: valid create info; the mapping lives as long as the buffer.
        let (buffer, memory, ptr) = unsafe {
            let buffer = gpu.device.create_buffer(&info, None)?;
            let reqs = gpu.device.get_buffer_memory_requirements(buffer);
            let flags = vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
            let memory = gpu.allocate(reqs, flags, None)?;
            gpu.device.bind_buffer_memory(buffer, memory, 0)?;
            let ptr = gpu.device.map_memory(memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty())?.cast::<u8>();
            (buffer, memory, ptr)
        };
        Ok(Self { gpu: gpu.clone(), buffer, memory, ptr, size })
    }

    pub fn write(&self, offset: usize, data: &[u8]) {
        assert!(offset + data.len() <= self.size, "write past the buffer");
        // SAFETY: in bounds of a live host-coherent mapping.
        unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), self.ptr.add(offset), data.len()) };
    }

    pub fn read(&self, offset: usize, len: usize) -> Vec<u8> {
        assert!(offset + len <= self.size, "read past the buffer");
        // SAFETY: in bounds of a live host-coherent mapping; the GPU work
        // that wrote it has completed (caller waited on its fence).
        unsafe { std::slice::from_raw_parts(self.ptr.add(offset), len).to_vec() }
    }
}

impl Drop for HostBuffer {
    fn drop(&mut self) {
        // SAFETY: owners wait for GPU work that uses the buffer before dropping.
        unsafe {
            self.gpu.device.unmap_memory(self.memory);
            self.gpu.device.destroy_buffer(self.buffer, None);
            self.gpu.device.free_memory(self.memory, None);
        }
    }
}
