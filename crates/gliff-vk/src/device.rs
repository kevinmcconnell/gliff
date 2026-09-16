//! Instance, device, queues and the small allocation/submission helpers the
//! rest of the crate builds on.

use std::ffi::{c_char, CStr};
use std::path::Path;
use std::sync::Arc;

use ash::vk;

use crate::{Error, Result};

/// Queue family indices; encode and decode are absent on GPUs without them.
#[derive(Debug, Clone, Copy)]
pub struct Families {
    pub compute: u32,
    pub encode: Option<u32>,
    pub decode: Option<u32>,
}

impl Families {
    /// Distinct families, for `SHARING_MODE_CONCURRENT` image creation.
    pub fn all(&self) -> Vec<u32> {
        let mut v = vec![self.compute];
        for f in [self.encode, self.decode].into_iter().flatten() {
            if !v.contains(&f) {
                v.push(f);
            }
        }
        v
    }
}

/// One Vulkan device with the queues, extension tables and pools the media
/// pipeline uses. Not thread-safe: every pipeline built on it lives on the
/// thread that created it, as the VA-API path did.
pub struct Gpu {
    pub(crate) instance: ash::Instance,
    pub(crate) physical: vk::PhysicalDevice,
    pub(crate) device: ash::Device,
    pub(crate) families: Families,
    pub(crate) compute_queue: vk::Queue,
    pub(crate) encode_queue: Option<vk::Queue>,
    pub(crate) decode_queue: Option<vk::Queue>,
    pub(crate) memory: vk::PhysicalDeviceMemoryProperties,
    pub(crate) video_instance: ash::khr::video_queue::Instance,
    pub(crate) video: ash::khr::video_queue::Device,
    pub(crate) encode: ash::khr::video_encode_queue::Device,
    pub(crate) decode: ash::khr::video_decode_queue::Device,
    pub(crate) external_fd: ash::khr::external_memory_fd::Device,
    pub(crate) drm_modifier: ash::ext::image_drm_format_modifier::Device,
    pub name: String,
    pub driver: String,
    _entry: ash::Entry,
}

const REQUIRED_EXTENSIONS: &[&CStr] = &[
    ash::khr::video_queue::NAME,
    ash::khr::external_memory_fd::NAME,
    ash::ext::external_memory_dma_buf::NAME,
    ash::ext::image_drm_format_modifier::NAME,
];

const OPTIONAL_EXTENSIONS: &[&CStr] = &[
    ash::khr::video_encode_queue::NAME,
    ash::khr::video_encode_h264::NAME,
    ash::khr::video_decode_queue::NAME,
    ash::khr::video_decode_h264::NAME,
];

/// Mesa's Intel driver (ANV) compiles Vulkan Video in but hides it until
/// `ANV_DEBUG` names `video-decode` and `video-encode`. gliff asks for both
/// itself so no user has to know. Any flags already in the variable stay.
/// Call this first thing in `main`, before the process has other threads.
pub fn prepare_driver_env() {
    const VAR: &str = "ANV_DEBUG";
    const WANTED: [&str; 2] = ["video-decode", "video-encode"];
    let current = std::env::var(VAR).unwrap_or_default();
    let mut flags: Vec<&str> = current
        .split(',')
        .map(str::trim)
        .filter(|f| !f.is_empty())
        .collect();
    let missing: Vec<&str> = WANTED
        .iter()
        .copied()
        .filter(|w| !flags.contains(w))
        .collect();
    if missing.is_empty() {
        return;
    }
    flags.extend(missing);
    let value = flags.join(",");
    tracing::debug!(%value, "setting {VAR} for Intel ANV video");
    std::env::set_var(VAR, value);
}

impl Gpu {
    /// Open the GPU behind `render_node` (any suitable one when `None`).
    pub fn open(render_node: Option<&Path>) -> Result<Arc<Self>> {
        // SAFETY: loading libvulkan and creating an instance with valid,
        // NUL-terminated names; nothing outlives the entry it came from.
        unsafe {
            let entry = ash::Entry::load()?;
            let app = vk::ApplicationInfo::default()
                .application_name(c"gliff")
                .api_version(vk::API_VERSION_1_3);
            let instance = entry.create_instance(
                &vk::InstanceCreateInfo::default().application_info(&app),
                None,
            )?;
            let wanted = render_node.and_then(drm_dev_number);
            let mut chosen = None;
            for pd in instance.enumerate_physical_devices()? {
                if let Some(want) = wanted {
                    let mut drm = vk::PhysicalDeviceDrmPropertiesEXT::default();
                    let mut props = vk::PhysicalDeviceProperties2::default().push_next(&mut drm);
                    instance.get_physical_device_properties2(pd, &mut props);
                    let matches = (drm.has_render == vk::TRUE
                        && (drm.render_major, drm.render_minor) == want)
                        || (drm.has_primary == vk::TRUE
                            && (drm.primary_major, drm.primary_minor) == want);
                    if !matches {
                        continue;
                    }
                }
                let exts: Vec<String> = instance
                    .enumerate_device_extension_properties(pd)?
                    .iter()
                    .map(|e| {
                        CStr::from_ptr(e.extension_name.as_ptr())
                            .to_string_lossy()
                            .into_owned()
                    })
                    .collect();
                let has = |n: &CStr| exts.iter().any(|e| e.as_str() == n.to_str().unwrap_or(""));
                if !REQUIRED_EXTENSIONS.iter().all(|n| has(n)) {
                    continue;
                }
                let Some(families) = pick_families(&instance, pd) else {
                    continue;
                };
                chosen = Some((pd, families, exts));
                break;
            }
            let Some((physical, families, exts)) = chosen else {
                instance.destroy_instance(None);
                return Err(Error::NoDevice(
                    "no Vulkan device with a compute queue, dmabuf import and video queues",
                ));
            };
            let has = |n: &CStr| exts.iter().any(|e| e.as_str() == n.to_str().unwrap_or(""));
            let mut names: Vec<*const c_char> =
                REQUIRED_EXTENSIONS.iter().map(|n| n.as_ptr()).collect();
            names.extend(
                OPTIONAL_EXTENSIONS
                    .iter()
                    .filter(|n| has(n))
                    .map(|n| n.as_ptr()),
            );

            let priority = [1.0f32];
            let queue_infos: Vec<vk::DeviceQueueCreateInfo> = families
                .all()
                .into_iter()
                .map(|f| {
                    vk::DeviceQueueCreateInfo::default()
                        .queue_family_index(f)
                        .queue_priorities(&priority)
                })
                .collect();
            let mut f12 = vk::PhysicalDeviceVulkan12Features::default().timeline_semaphore(true);
            let mut f13 = vk::PhysicalDeviceVulkan13Features::default().synchronization2(true);
            let create = vk::DeviceCreateInfo::default()
                .queue_create_infos(&queue_infos)
                .enabled_extension_names(&names)
                .push_next(&mut f12)
                .push_next(&mut f13);
            let device = instance.create_device(physical, &create, None)?;

            let props = instance.get_physical_device_properties(physical);
            let mut drv = vk::PhysicalDeviceDriverProperties::default();
            let mut p2 = vk::PhysicalDeviceProperties2::default().push_next(&mut drv);
            instance.get_physical_device_properties2(physical, &mut p2);
            let name = CStr::from_ptr(props.device_name.as_ptr())
                .to_string_lossy()
                .into_owned();
            let driver = format!(
                "{} {}",
                CStr::from_ptr(drv.driver_name.as_ptr()).to_string_lossy(),
                CStr::from_ptr(drv.driver_info.as_ptr()).to_string_lossy()
            );
            tracing::info!(%name, %driver, ?families, "vulkan device");

            let gpu = Self {
                compute_queue: device.get_device_queue(families.compute, 0),
                encode_queue: families.encode.map(|f| device.get_device_queue(f, 0)),
                decode_queue: families.decode.map(|f| device.get_device_queue(f, 0)),
                memory: instance.get_physical_device_memory_properties(physical),
                video_instance: ash::khr::video_queue::Instance::new(&entry, &instance),
                video: ash::khr::video_queue::Device::new(&instance, &device),
                encode: ash::khr::video_encode_queue::Device::new(&instance, &device),
                decode: ash::khr::video_decode_queue::Device::new(&instance, &device),
                external_fd: ash::khr::external_memory_fd::Device::new(&instance, &device),
                drm_modifier: ash::ext::image_drm_format_modifier::Device::new(&instance, &device),
                name,
                driver,
                families,
                instance,
                physical,
                device,
                _entry: entry,
            };
            Ok(Arc::new(gpu))
        }
    }

    pub fn can_encode(&self) -> bool {
        self.encode_queue.is_some()
    }

    pub fn can_decode(&self) -> bool {
        self.decode_queue.is_some()
    }

    pub(crate) fn encode_family(&self) -> Result<u32> {
        self.families
            .encode
            .ok_or_else(|| Error::Unsupported("no video encode queue".into()))
    }

    pub(crate) fn decode_family(&self) -> Result<u32> {
        self.families
            .decode
            .ok_or_else(|| Error::Unsupported("no video decode queue".into()))
    }

    /// Index of a memory type allowed by `type_bits` with all of `flags`.
    pub(crate) fn memory_type(
        &self,
        type_bits: u32,
        flags: vk::MemoryPropertyFlags,
    ) -> Result<u32> {
        (0..self.memory.memory_type_count)
            .find(|&i| {
                type_bits & (1 << i) != 0
                    && self.memory.memory_types[i as usize]
                        .property_flags
                        .contains(flags)
            })
            .ok_or_else(|| Error::Unsupported(format!("no memory type for {flags:?}")))
    }

    /// Allocate memory for `reqs`, with an optional extra create-info chain
    /// (import/export/dedicated).
    pub(crate) fn allocate(
        &self,
        reqs: vk::MemoryRequirements,
        flags: vk::MemoryPropertyFlags,
        chain: Option<&mut dyn vk::ExtendsMemoryAllocateInfo>,
    ) -> Result<vk::DeviceMemory> {
        let mut info = vk::MemoryAllocateInfo::default()
            .allocation_size(reqs.size)
            .memory_type_index(self.memory_type(reqs.memory_type_bits, flags)?);
        if let Some(c) = chain {
            info = info.push_next(c);
        }
        // SAFETY: a valid allocate info; the caller frees the memory.
        Ok(unsafe { self.device.allocate_memory(&info, None) }?)
    }

    /// Wait for the whole device to go idle (teardown, resize).
    pub fn wait_idle(&self) {
        // SAFETY: valid device.
        let _ = unsafe { self.device.device_wait_idle() };
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        // SAFETY: every object created from this device was destroyed by its
        // owner before the Arc<Gpu> count reached zero.
        unsafe {
            let _ = self.device.device_wait_idle();
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}

fn drm_dev_number(path: &Path) -> Option<(i64, i64)> {
    use std::os::unix::fs::MetadataExt;
    let rdev = std::fs::metadata(path).ok()?.rdev();
    // Linux dev_t layout: major in bits 8..20 (and 32..), minor in 0..8 and 20..32.
    let major = ((rdev >> 8) & 0xfff) | ((rdev >> 32) & !0xfff);
    let minor = (rdev & 0xff) | ((rdev >> 12) & !0xff);
    Some((major as i64, minor as i64))
}

/// Choose a compute family (no graphics preferred, so it never competes with
/// the compositor) and the video families.
fn pick_families(instance: &ash::Instance, pd: vk::PhysicalDevice) -> Option<Families> {
    // SAFETY: valid physical device; the structs are properly chained.
    let (props, video) = unsafe {
        let n = instance
            .get_physical_device_queue_family_properties(pd)
            .len();
        let mut video = vec![vk::QueueFamilyVideoPropertiesKHR::default(); n];
        let mut props = vec![vk::QueueFamilyProperties2::default(); n];
        for (p, v) in props.iter_mut().zip(video.iter_mut()) {
            p.p_next = (v as *mut vk::QueueFamilyVideoPropertiesKHR).cast();
        }
        instance.get_physical_device_queue_family_properties2(pd, &mut props);
        (props, video)
    };
    let flags = |i: usize| props[i].queue_family_properties.queue_flags;
    let compute = (0..props.len())
        .find(|&i| {
            flags(i).contains(vk::QueueFlags::COMPUTE)
                && !flags(i).contains(vk::QueueFlags::GRAPHICS)
        })
        .or_else(|| (0..props.len()).find(|&i| flags(i).contains(vk::QueueFlags::COMPUTE)))?;
    let encode = (0..props.len()).find(|&i| {
        video[i]
            .video_codec_operations
            .contains(vk::VideoCodecOperationFlagsKHR::ENCODE_H264)
    });
    let decode = (0..props.len()).find(|&i| {
        video[i]
            .video_codec_operations
            .contains(vk::VideoCodecOperationFlagsKHR::DECODE_H264)
    });
    Some(Families {
        compute: compute as u32,
        encode: encode.map(|i| i as u32),
        decode: decode.map(|i| i as u32),
    })
}

/// A command pool plus a fence, for one-shot submissions on one queue.
pub(crate) struct Commands {
    gpu: Arc<Gpu>,
    pool: vk::CommandPool,
    pub(crate) queue: vk::Queue,
    fence: vk::Fence,
}

impl Commands {
    pub(crate) fn new(gpu: &Arc<Gpu>, family: u32, queue: vk::Queue) -> Result<Self> {
        // SAFETY: valid device and family index. The fence starts signalled
        // so the first `run` does not wait on work that was never submitted.
        let (pool, fence) = unsafe {
            let info = vk::CommandPoolCreateInfo::default()
                .queue_family_index(family)
                .flags(vk::CommandPoolCreateFlags::TRANSIENT);
            let fence = vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED);
            (
                gpu.device.create_command_pool(&info, None)?,
                gpu.device.create_fence(&fence, None)?,
            )
        };
        Ok(Self {
            gpu: gpu.clone(),
            pool,
            queue,
            fence,
        })
    }

    /// Record with `f` and submit. Waits for `wait` (a timeline value) first
    /// and signals `signal` after; when `block` the call returns only once
    /// the work has finished.
    pub(crate) fn run(
        &self,
        timeline: vk::Semaphore,
        wait: Option<u64>,
        signal: Option<u64>,
        block: bool,
        f: impl FnOnce(vk::CommandBuffer) -> Result<()>,
    ) -> Result<()> {
        let dev = &self.gpu.device;
        // SAFETY: the pool is reset only after the previous submission's fence
        // signalled (`block`, or the wait below), so no command buffer is in
        // flight when it is recycled.
        unsafe {
            dev.wait_for_fences(&[self.fence], true, u64::MAX)?;
            dev.reset_fences(&[self.fence])?;
            dev.reset_command_pool(self.pool, vk::CommandPoolResetFlags::empty())?;
            let alloc = vk::CommandBufferAllocateInfo::default()
                .command_pool(self.pool)
                .level(vk::CommandBufferLevel::PRIMARY)
                .command_buffer_count(1);
            let cmd = dev.allocate_command_buffers(&alloc)?[0];
            dev.begin_command_buffer(
                cmd,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?;
            f(cmd)?;
            dev.end_command_buffer(cmd)?;

            let waits: Vec<vk::SemaphoreSubmitInfo> = wait
                .map(|v| {
                    vk::SemaphoreSubmitInfo::default()
                        .semaphore(timeline)
                        .value(v)
                        .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                })
                .into_iter()
                .collect();
            let signals: Vec<vk::SemaphoreSubmitInfo> = signal
                .map(|v| {
                    vk::SemaphoreSubmitInfo::default()
                        .semaphore(timeline)
                        .value(v)
                        .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                })
                .into_iter()
                .collect();
            let cmds = [vk::CommandBufferSubmitInfo::default().command_buffer(cmd)];
            let submit = vk::SubmitInfo2::default()
                .wait_semaphore_infos(&waits)
                .command_buffer_infos(&cmds)
                .signal_semaphore_infos(&signals);
            dev.queue_submit2(self.queue, &[submit], self.fence)?;
            if block {
                dev.wait_for_fences(&[self.fence], true, u64::MAX)?;
            }
        }
        Ok(())
    }

    pub(crate) fn wait(&self) -> Result<()> {
        // SAFETY: valid fence.
        unsafe {
            self.gpu
                .device
                .wait_for_fences(&[self.fence], true, u64::MAX)?
        };
        Ok(())
    }
}

impl Drop for Commands {
    fn drop(&mut self) {
        // SAFETY: wait for in-flight work before destroying the pool.
        unsafe {
            let _ = self
                .gpu
                .device
                .wait_for_fences(&[self.fence], true, u64::MAX);
            self.gpu.device.destroy_fence(self.fence, None);
            self.gpu.device.destroy_command_pool(self.pool, None);
        }
    }
}

/// A timeline semaphore that orders compute and video work across queues.
pub(crate) struct Timeline {
    gpu: Arc<Gpu>,
    pub(crate) semaphore: vk::Semaphore,
    next: u64,
}

impl Timeline {
    pub(crate) fn new(gpu: &Arc<Gpu>) -> Result<Self> {
        let mut kind = vk::SemaphoreTypeCreateInfo::default()
            .semaphore_type(vk::SemaphoreType::TIMELINE)
            .initial_value(0);
        let info = vk::SemaphoreCreateInfo::default().push_next(&mut kind);
        // SAFETY: valid device and create info.
        let semaphore = unsafe { gpu.device.create_semaphore(&info, None) }?;
        Ok(Self {
            gpu: gpu.clone(),
            semaphore,
            next: 0,
        })
    }

    /// The next value to signal.
    pub(crate) fn advance(&mut self) -> u64 {
        self.next += 1;
        self.next
    }
}

impl Drop for Timeline {
    fn drop(&mut self) {
        // SAFETY: owners wait for their work before dropping.
        unsafe { self.gpu.device.destroy_semaphore(self.semaphore, None) };
    }
}
