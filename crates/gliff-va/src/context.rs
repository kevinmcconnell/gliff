//! Codec configs and contexts, and the parameter and coded buffers that go
//! through them.

use std::sync::Arc;

use crate::bindings as va;
use crate::display::Display;
use crate::surface::Surface;
use crate::{check, Result};

/// A `VAConfigID`: profile, entrypoint and the attributes gliff asks for.
pub struct Config {
    display: Arc<Display>,
    pub id: va::VAConfigID,
}

impl Config {
    pub fn new(
        display: &Arc<Display>,
        profile: va::VAProfile,
        entrypoint: va::VAEntrypoint,
        attribs: &[(va::VAConfigAttribType, u32)],
    ) -> Result<Self> {
        let mut list: Vec<va::VAConfigAttrib> = attribs
            .iter()
            .map(|&(type_, value)| va::VAConfigAttrib { type_, value })
            .collect();
        let mut id = 0;
        // SAFETY: the attribute array is sized by its length; one id is
        // written back.
        unsafe {
            check(
                va::vaCreateConfig(
                    display.raw(),
                    profile,
                    entrypoint,
                    list.as_mut_ptr(),
                    list.len() as i32,
                    &mut id,
                ),
                "vaCreateConfig",
            )?;
        }
        Ok(Self {
            display: display.clone(),
            id,
        })
    }
}

impl Drop for Config {
    fn drop(&mut self) {
        // SAFETY: the id came from vaCreateConfig on this display.
        unsafe {
            va::vaDestroyConfig(self.display.raw(), self.id);
        }
    }
}

/// A `VAContextID` for one coded size.
pub struct Context {
    display: Arc<Display>,
    pub id: va::VAContextID,
}

impl Context {
    /// `targets` lists the surfaces the context may render to; some drivers
    /// want the full set up front.
    pub fn new(
        display: &Arc<Display>,
        config: &Config,
        width: u32,
        height: u32,
        targets: &[&Surface],
    ) -> Result<Self> {
        let mut ids: Vec<va::VASurfaceID> = targets.iter().map(|s| s.id).collect();
        let mut id = 0;
        // SAFETY: the surface array is sized by its length; one id is
        // written back.
        unsafe {
            check(
                va::vaCreateContext(
                    display.raw(),
                    config.id,
                    width as i32,
                    height as i32,
                    va::VA_PROGRESSIVE as i32,
                    ids.as_mut_ptr(),
                    ids.len() as i32,
                    &mut id,
                ),
                "vaCreateContext",
            )?;
        }
        Ok(Self {
            display: display.clone(),
            id,
        })
    }

    /// Run one picture: begin on `target`, render every buffer, end. The
    /// driver may still be working when this returns.
    pub fn render(&self, target: &Surface, buffers: &[Buffer]) -> Result<()> {
        let mut ids: Vec<va::VABufferID> = buffers.iter().map(|b| b.id).collect();
        // SAFETY: valid context, surface and buffer ids on this display.
        unsafe {
            check(
                va::vaBeginPicture(self.display.raw(), self.id, target.id),
                "vaBeginPicture",
            )?;
            check(
                va::vaRenderPicture(
                    self.display.raw(),
                    self.id,
                    ids.as_mut_ptr(),
                    ids.len() as i32,
                ),
                "vaRenderPicture",
            )?;
            check(
                va::vaEndPicture(self.display.raw(), self.id),
                "vaEndPicture",
            )
        }
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        // SAFETY: the id came from vaCreateContext on this display.
        unsafe {
            va::vaDestroyContext(self.display.raw(), self.id);
        }
    }
}

/// A parameter or data buffer, freed on drop.
pub struct Buffer {
    display: Arc<Display>,
    pub id: va::VABufferID,
}

impl Buffer {
    /// A buffer holding one plain struct.
    pub fn new<T: Copy>(context: &Context, type_: va::VABufferType, value: &T) -> Result<Self> {
        Self::from_bytes(
            context,
            type_,
            std::mem::size_of::<T>(),
            1,
            (value as *const T).cast(),
        )
    }

    /// A buffer holding raw bytes (bitstream data, packed headers).
    pub fn data(context: &Context, type_: va::VABufferType, bytes: &[u8]) -> Result<Self> {
        Self::from_bytes(context, type_, bytes.len(), 1, bytes.as_ptr().cast())
    }

    /// A `VAEncMiscParameterBuffer` wrapping `value`.
    pub fn misc<T: Copy>(
        context: &Context,
        misc_type: va::VAEncMiscParameterType,
        value: &T,
    ) -> Result<Self> {
        let mut bytes = Vec::with_capacity(4 + std::mem::size_of::<T>());
        bytes.extend_from_slice(&misc_type.to_ne_bytes());
        // SAFETY: T is a plain C struct; its bytes are copied into the vector.
        bytes.extend_from_slice(unsafe {
            std::slice::from_raw_parts((value as *const T).cast::<u8>(), std::mem::size_of::<T>())
        });
        Self::from_bytes(
            context,
            va::VAEncMiscParameterBufferType,
            bytes.len(),
            1,
            bytes.as_ptr().cast(),
        )
    }

    /// An empty coded buffer of `size` bytes.
    pub fn coded(context: &Context, size: usize) -> Result<Self> {
        Self::from_bytes(context, va::VAEncCodedBufferType, size, 1, std::ptr::null())
    }

    fn from_bytes(
        context: &Context,
        type_: va::VABufferType,
        size: usize,
        count: u32,
        data: *const std::ffi::c_void,
    ) -> Result<Self> {
        let mut id = 0;
        // SAFETY: `data` points at `size` readable bytes, or is null for an
        // uninitialised buffer; the driver copies what it needs.
        unsafe {
            check(
                va::vaCreateBuffer(
                    context.display.raw(),
                    context.id,
                    type_,
                    size as u32,
                    count,
                    data.cast_mut(),
                    &mut id,
                ),
                "vaCreateBuffer",
            )?;
        }
        Ok(Self {
            display: context.display.clone(),
            id,
        })
    }

    /// Wait for the encode that writes this coded buffer and collect its
    /// bytes in segment order. Returns the data and the OR of the segment
    /// status words.
    pub fn read_coded(&self) -> Result<(Vec<u8>, u32)> {
        // SAFETY: valid buffer id; the mapped segment list is only read while
        // mapped, and each segment's `buf` points at `size` bytes.
        unsafe {
            check(
                va::vaSyncBuffer(self.display.raw(), self.id, u64::MAX),
                "vaSyncBuffer",
            )?;
            let mut ptr: *mut std::ffi::c_void = std::ptr::null_mut();
            check(
                va::vaMapBuffer(self.display.raw(), self.id, &mut ptr),
                "vaMapBuffer",
            )?;
            let mut out = Vec::new();
            let mut status = 0;
            let mut seg = ptr.cast::<va::VACodedBufferSegment>();
            let mut guard = 0;
            while !seg.is_null() && guard < 1024 {
                let s = &*seg;
                if !s.buf.is_null() {
                    out.extend_from_slice(std::slice::from_raw_parts(
                        s.buf.cast::<u8>(),
                        s.size as usize,
                    ));
                }
                status |= s.status;
                seg = s.next.cast();
                guard += 1;
            }
            va::vaUnmapBuffer(self.display.raw(), self.id);
            Ok((out, status))
        }
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        // SAFETY: the id came from vaCreateBuffer on this display.
        unsafe {
            va::vaDestroyBuffer(self.display.raw(), self.id);
        }
    }
}
