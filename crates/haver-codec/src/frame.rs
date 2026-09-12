//! NV12 frames in dmabufs, shared between the encoder, decoder, and GL.

use std::fs::File;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::Path;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use cros_codecs::libva::{Display, Surface};
use cros_codecs::video_frame::generic_dma_video_frame::GenericDmaVideoFrame;
use cros_codecs::video_frame::{ReadMapping, VideoFrame, WriteMapping};
use cros_codecs::{FrameLayout, Fourcc, PlaneLayout, Resolution};
use gbm::{BufferObjectFlags, Device, Format};

use crate::{Error, Result};

pub fn nv12() -> Fourcc {
    Fourcc::from(b"NV12")
}

/// One plane of a dmabuf: byte offset and stride inside the single buffer object.
#[derive(Debug, Clone, Copy)]
pub struct Plane {
    pub offset: u32,
    pub stride: u32,
}

/// Everything a consumer (EGL, VA-API, a CPU mapper) needs to reach the pixels.
#[derive(Debug)]
pub struct DmabufInfo {
    pub fd: OwnedFd,
    pub width: u32,
    pub height: u32,
    pub fourcc: u32,
    pub modifier: u64,
    pub planes: Vec<Plane>,
}

impl DmabufInfo {
    pub fn fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// A pooled NV12 dmabuf frame. Implements cros-codecs' `VideoFrame` so it can be
/// fed to the encoder and handed to the decoder as an output buffer.
#[derive(Debug)]
pub struct Nv12Frame {
    id: u64,
    inner: Option<GenericDmaVideoFrame>,
    info: Arc<DmabufInfo>,
    pool: Weak<Mutex<Vec<(GenericDmaVideoFrame, Arc<DmabufInfo>, u64)>>>,
}

impl Nv12Frame {
    /// Stable identity of the underlying buffer; the same buffer keeps its id
    /// when it cycles through the pool, so importers can cache by it.
    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn info(&self) -> &Arc<DmabufInfo> {
        &self.info
    }

    pub fn width(&self) -> u32 {
        self.info.width
    }

    pub fn height(&self) -> u32 {
        self.info.height
    }

    pub fn layout(&self) -> FrameLayout {
        FrameLayout {
            format: (nv12(), self.info.modifier),
            size: Resolution { width: self.info.width, height: self.info.height },
            planes: self
                .info
                .planes
                .iter()
                .map(|p| PlaneLayout { buffer_index: 0, offset: p.offset as usize, stride: p.stride as usize })
                .collect(),
        }
    }

    fn inner(&self) -> &GenericDmaVideoFrame {
        self.inner.as_ref().expect("frame inner present until drop")
    }

    /// Run `f` with writable Y and UV plane slices.
    pub fn with_planes_mut<R>(&mut self, f: impl FnOnce(&mut [u8], &mut [u8], usize, usize) -> R) -> Result<R> {
        let pitches = self.inner().get_plane_pitch();
        let mapping = self
            .inner
            .as_mut()
            .expect("frame inner present until drop")
            .map_mut()
            .map_err(Error::Gbm)?;
        let planes = mapping.get();
        let mut y = planes[0].borrow_mut();
        let mut uv = planes[1].borrow_mut();
        Ok(f(&mut y, &mut uv, pitches[0], pitches[1]))
    }

    /// Run `f` with read-only Y and UV plane slices.
    pub fn with_planes<R>(&self, f: impl FnOnce(&[u8], &[u8], usize, usize) -> R) -> Result<R> {
        let pitches = self.inner().get_plane_pitch();
        let mapping = self.inner().map().map_err(Error::Gbm)?;
        let planes = mapping.get();
        Ok(f(planes[0], planes[1], pitches[0], pitches[1]))
    }
}

impl Drop for Nv12Frame {
    fn drop(&mut self) {
        if let (Some(inner), Some(pool)) = (self.inner.take(), self.pool.upgrade()) {
            if let Ok(mut free) = pool.lock() {
                free.push((inner, Arc::clone(&self.info), self.id));
            }
        }
    }
}

impl VideoFrame for Nv12Frame {
    type MemDescriptor = GenericDmaVideoFrame;
    type NativeHandle = Surface<GenericDmaVideoFrame>;

    fn fourcc(&self) -> Fourcc {
        nv12()
    }

    fn resolution(&self) -> Resolution {
        Resolution { width: self.info.width, height: self.info.height }
    }

    fn get_plane_size(&self) -> Vec<usize> {
        self.inner().get_plane_size()
    }

    fn get_plane_pitch(&self) -> Vec<usize> {
        self.inner().get_plane_pitch()
    }

    fn map<'a>(&'a self) -> std::result::Result<Box<dyn ReadMapping<'a> + 'a>, String> {
        self.inner().map()
    }

    fn map_mut<'a>(&'a mut self) -> std::result::Result<Box<dyn WriteMapping<'a> + 'a>, String> {
        self.inner.as_mut().expect("frame inner present until drop").map_mut()
    }

    fn to_native_handle(&self, display: &Rc<Display>) -> std::result::Result<Self::NativeHandle, String> {
        self.inner().to_native_handle(display)
    }
}

/// Allocates linear NV12 dmabufs with GBM.
pub struct FrameAllocator {
    device: Device<File>,
}

impl FrameAllocator {
    pub fn open(render_node: &Path) -> Result<Self> {
        let file = File::options().read(true).write(true).open(render_node)?;
        let device = Device::new(file)?;
        Ok(Self { device })
    }

    fn allocate(&self, width: u32, height: u32) -> Result<(GenericDmaVideoFrame, Arc<DmabufInfo>)> {
        let bo = self
            .device
            .create_buffer_object::<()>(width, height, Format::Nv12, BufferObjectFlags::LINEAR)
            .map_err(|e| Error::Gbm(format!("create NV12 {width}x{height}: {e}")))?;
        let plane_count = bo.plane_count().map_err(|e| Error::Gbm(e.to_string()))?;
        if plane_count != 2 {
            return Err(Error::Gbm(format!("NV12 buffer has {plane_count} planes, expected 2")));
        }
        let modifier: u64 = bo.modifier().map_err(|e| Error::Gbm(e.to_string()))?.into();
        let mut planes = Vec::with_capacity(2);
        for i in 0..2 {
            planes.push(Plane {
                offset: bo.offset(i).map_err(|e| Error::Gbm(e.to_string()))?,
                stride: bo.stride_for_plane(i).map_err(|e| Error::Gbm(e.to_string()))?,
            });
        }
        let fd = bo.fd().map_err(|e| Error::Gbm(e.to_string()))?;
        let info = Arc::new(DmabufInfo {
            fd,
            width,
            height,
            fourcc: Format::Nv12 as u32,
            modifier,
            planes,
        });
        let file = File::from(info.fd.try_clone()?);
        let layout = FrameLayout {
            format: (nv12(), modifier),
            size: Resolution { width, height },
            planes: info
                .planes
                .iter()
                .map(|p| PlaneLayout { buffer_index: 0, offset: p.offset as usize, stride: p.stride as usize })
                .collect(),
        };
        let inner = GenericDmaVideoFrame::new(vec![file], layout).map_err(Error::Gbm)?;
        Ok((inner, info))
    }
}

type FreeList = Arc<Mutex<Vec<(GenericDmaVideoFrame, Arc<DmabufInfo>, u64)>>>;

/// Fixed-size pool of NV12 frames of one resolution. Frames return on drop.
pub struct FramePool {
    width: u32,
    height: u32,
    free: FreeList,
    total: usize,
}

impl FramePool {
    pub fn new(allocator: &FrameAllocator, width: u32, height: u32, count: usize) -> Result<Self> {
        let mut free = Vec::with_capacity(count);
        for _ in 0..count {
            let (inner, info) = allocator.allocate(width, height)?;
            free.push((inner, info, NEXT_ID.fetch_add(1, Ordering::Relaxed)));
        }
        Ok(Self { width, height, free: Arc::new(Mutex::new(free)), total: count })
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn capacity(&self) -> usize {
        self.total
    }

    pub fn available(&self) -> usize {
        self.free.lock().map(|f| f.len()).unwrap_or(0)
    }

    pub fn alloc(&self) -> Option<Nv12Frame> {
        let (inner, info, id) = self.free.lock().ok()?.pop()?;
        Some(Nv12Frame { id, inner: Some(inner), info, pool: Arc::downgrade(&self.free) })
    }

    pub fn try_alloc(&self) -> Result<Nv12Frame> {
        self.alloc().ok_or(Error::PoolEmpty)
    }
}

/// Round up to a multiple of `align` (a power of two).
pub fn align_up(v: u32, align: u32) -> u32 {
    (v + align - 1) & !(align - 1)
}
