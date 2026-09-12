//! Cursor image buffers: a wl_shm pool over a memfd, read back with `read_at`.

use std::fs::File;
use std::os::fd::AsFd;
use std::os::unix::fs::FileExt;

use nix::sys::memfd::{memfd_create, MFdFlags};
use wayland_client::protocol::wl_buffer::WlBuffer;
use wayland_client::protocol::wl_shm::{self, WlShm};
use wayland_client::protocol::wl_shm_pool::WlShmPool;
use wayland_client::{Dispatch, QueueHandle};

use crate::{Error, Result};

pub struct ShmBuffer {
    file: File,
    pool: WlShmPool,
    pub buffer: WlBuffer,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
}

impl ShmBuffer {
    pub fn new<D>(shm: &WlShm, qh: &QueueHandle<D>, width: u32, height: u32) -> Result<Self>
    where
        D: Dispatch<WlShmPool, ()> + Dispatch<WlBuffer, ()> + 'static,
    {
        let stride = width * 4;
        let size = (stride * height) as u64;
        let fd = memfd_create(c"haver-cursor", MFdFlags::MFD_CLOEXEC)
            .map_err(|e| Error::Capture(format!("memfd: {e}")))?;
        let file = File::from(fd);
        file.set_len(size)?;
        let pool = shm.create_pool(file.as_fd(), size as i32, qh, ());
        let buffer = pool.create_buffer(
            0,
            width as i32,
            height as i32,
            stride as i32,
            wl_shm::Format::Argb8888,
            qh,
            (),
        );
        Ok(Self {
            file,
            pool,
            buffer,
            width,
            height,
            stride,
        })
    }

    /// Copy the pixels out as tightly packed ARGB8888 (little-endian BGRA bytes).
    pub fn read_argb(&self) -> Result<Vec<u8>> {
        let mut out = vec![0u8; (self.width * self.height * 4) as usize];
        for row in 0..self.height as usize {
            let off = row as u64 * self.stride as u64;
            let dst = &mut out[row * self.width as usize * 4..(row + 1) * self.width as usize * 4];
            self.file.read_exact_at(dst, off)?;
        }
        Ok(out)
    }
}

impl Drop for ShmBuffer {
    fn drop(&mut self) {
        self.buffer.destroy();
        self.pool.destroy();
    }
}
