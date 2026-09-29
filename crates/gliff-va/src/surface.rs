//! Driver-owned NV12 surfaces and their dmabuf export.

use std::os::fd::{FromRawFd, OwnedFd};
use std::sync::Arc;

use crate::bindings as va;
use crate::display::Display;
use crate::{check, Error, Result};

/// What a surface is for; the driver picks a layout accordingly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageHint {
    Encoder,
    Decoder,
}

/// One NV12 surface the driver allocated.
pub struct Surface {
    display: Arc<Display>,
    pub id: va::VASurfaceID,
    pub width: u32,
    pub height: u32,
}

/// One plane of an exported surface: which object it lives in and where.
#[derive(Debug, Clone, Copy)]
pub struct PrimePlane {
    pub object: usize,
    pub drm_format: u32,
    pub offset: u32,
    pub pitch: u32,
}

/// An exported surface: the dmabuf objects (owned here, closed on drop),
/// the layout of each plane, and the DRM format modifier they share.
#[derive(Debug)]
pub struct PrimeDescriptor {
    pub fourcc: u32,
    pub width: u32,
    pub height: u32,
    pub modifier: u64,
    pub objects: Vec<OwnedFd>,
    pub object_sizes: Vec<u32>,
    pub planes: Vec<PrimePlane>,
}

impl Surface {
    /// Allocate an NV12 surface of `width` x `height`.
    pub fn new_nv12(
        display: &Arc<Display>,
        width: u32,
        height: u32,
        hint: UsageHint,
    ) -> Result<Self> {
        let hint = match hint {
            UsageHint::Encoder => va::VA_SURFACE_ATTRIB_USAGE_HINT_ENCODER,
            UsageHint::Decoder => va::VA_SURFACE_ATTRIB_USAGE_HINT_DECODER,
        };
        let mut attribs = [
            int_attrib(va::VASurfaceAttribPixelFormat, va::VA_FOURCC_NV12 as i32),
            int_attrib(va::VASurfaceAttribUsageHint, hint as i32),
        ];
        let mut id: va::VASurfaceID = va::VA_INVALID_SURFACE;
        // SAFETY: one surface id is written; the attribute array is sized by
        // its length.
        unsafe {
            check(
                va::vaCreateSurfaces(
                    display.raw(),
                    va::VA_RT_FORMAT_YUV420,
                    width,
                    height,
                    &mut id,
                    1,
                    attribs.as_mut_ptr(),
                    attribs.len() as u32,
                ),
                "vaCreateSurfaces",
            )?;
        }
        Ok(Self {
            display: display.clone(),
            id,
            width,
            height,
        })
    }

    /// Export the surface as a dmabuf, one layer per plane, for another API
    /// to read and write.
    pub fn export(&self) -> Result<PrimeDescriptor> {
        let mut desc = va::VADRMPRIMESurfaceDescriptor {
            fourcc: 0,
            width: 0,
            height: 0,
            num_objects: 0,
            objects: [va::_VADRMPRIMESurfaceDescriptor__bindgen_ty_1 {
                fd: -1,
                size: 0,
                drm_format_modifier: 0,
            }; 4],
            num_layers: 0,
            layers: [va::_VADRMPRIMESurfaceDescriptor__bindgen_ty_2 {
                drm_format: 0,
                num_planes: 0,
                object_index: [0; 4],
                offset: [0; 4],
                pitch: [0; 4],
            }; 4],
        };
        // SAFETY: the descriptor is a plain struct libva fills in; the fds it
        // returns are owned by the caller and wrapped as OwnedFd below.
        unsafe {
            check(
                va::vaExportSurfaceHandle(
                    self.display.raw(),
                    self.id,
                    va::VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2,
                    va::VA_EXPORT_SURFACE_SEPARATE_LAYERS | va::VA_EXPORT_SURFACE_READ_WRITE,
                    (&mut desc as *mut va::VADRMPRIMESurfaceDescriptor).cast(),
                ),
                "vaExportSurfaceHandle",
            )?;
        }
        let num_objects = (desc.num_objects as usize).min(4);
        let objects: Vec<OwnedFd> = desc.objects[..num_objects]
            .iter()
            // SAFETY: each fd was just handed to us by libva and is not
            // owned by anything else.
            .map(|o| unsafe { OwnedFd::from_raw_fd(o.fd) })
            .collect();
        let object_sizes = desc.objects[..num_objects].iter().map(|o| o.size).collect();
        let modifier = desc.objects[0].drm_format_modifier;
        if desc.objects[..num_objects]
            .iter()
            .any(|o| o.drm_format_modifier != modifier)
        {
            return Err(Error::Unsupported(
                "exported surface objects carry different modifiers".into(),
            ));
        }
        let mut planes = Vec::new();
        for layer in &desc.layers[..(desc.num_layers as usize).min(4)] {
            for p in 0..(layer.num_planes as usize).min(4) {
                planes.push(PrimePlane {
                    object: layer.object_index[p] as usize,
                    drm_format: layer.drm_format,
                    offset: layer.offset[p],
                    pitch: layer.pitch[p],
                });
            }
        }
        Ok(PrimeDescriptor {
            fourcc: desc.fourcc,
            width: desc.width,
            height: desc.height,
            modifier,
            objects,
            object_sizes,
            planes,
        })
    }

    /// Wait until every operation on the surface has finished.
    pub fn sync(&self) -> Result<()> {
        // SAFETY: valid display and surface.
        unsafe {
            check(
                va::vaSyncSurface(self.display.raw(), self.id),
                "vaSyncSurface",
            )
        }
    }

    /// Copy the pixels out through the driver: Y plane then packed UV, each
    /// tightly packed at the surface size.
    pub fn read_nv12(&self) -> Result<(Vec<u8>, Vec<u8>)> {
        let mut image = self.new_image()?;
        // SAFETY: the image was created for this surface size; the mapped
        // pointer is only read while mapped.
        let planes = unsafe {
            check(
                va::vaGetImage(
                    self.display.raw(),
                    self.id,
                    0,
                    0,
                    self.width,
                    self.height,
                    image.id.image_id,
                ),
                "vaGetImage",
            )?;
            let mapped = image.map()?;
            let copy = |plane: usize, rows: u32, row_bytes: usize| {
                let mut out = Vec::with_capacity(rows as usize * row_bytes);
                for r in 0..rows as usize {
                    let start =
                        image.id.offsets[plane] as usize + r * image.id.pitches[plane] as usize;
                    out.extend_from_slice(std::slice::from_raw_parts(mapped.add(start), row_bytes));
                }
                out
            };
            let y = copy(0, self.height, self.width as usize);
            let uv = copy(
                1,
                self.height.div_ceil(2),
                (self.width as usize).div_ceil(2) * 2,
            );
            (y, uv)
        };
        image.unmap();
        Ok(planes)
    }

    /// Upload tightly packed Y and UV planes through the driver.
    pub fn write_nv12(&self, y: &[u8], uv: &[u8]) -> Result<()> {
        let mut image = self.new_image()?;
        let (w, h) = (self.width as usize, self.height as usize);
        if y.len() < w * h || uv.len() < w.div_ceil(2) * 2 * h.div_ceil(2) {
            return Err(Error::Unsupported("plane data too short".into()));
        }
        // SAFETY: the mapped buffer covers `data_size`; every write stays
        // inside the plane rows the pitches describe.
        unsafe {
            let mapped = image.map()?;
            for r in 0..h {
                let dst =
                    mapped.add(image.id.offsets[0] as usize + r * image.id.pitches[0] as usize);
                std::ptr::copy_nonoverlapping(y[r * w..].as_ptr(), dst, w);
            }
            let uv_w = w.div_ceil(2) * 2;
            for r in 0..h.div_ceil(2) {
                let dst =
                    mapped.add(image.id.offsets[1] as usize + r * image.id.pitches[1] as usize);
                std::ptr::copy_nonoverlapping(uv[r * uv_w..].as_ptr(), dst, uv_w);
            }
            image.unmap();
            check(
                va::vaPutImage(
                    self.display.raw(),
                    self.id,
                    image.id.image_id,
                    0,
                    0,
                    self.width,
                    self.height,
                    0,
                    0,
                    self.width,
                    self.height,
                ),
                "vaPutImage",
            )
        }
    }

    fn new_image(&self) -> Result<VaImage> {
        let mut format = va::VAImageFormat {
            fourcc: va::VA_FOURCC_NV12,
            byte_order: 1,
            bits_per_pixel: 12,
            depth: 0,
            red_mask: 0,
            green_mask: 0,
            blue_mask: 0,
            alpha_mask: 0,
            va_reserved: [0; 4],
        };
        let mut id: va::VAImage = va::VAImage {
            image_id: va::VA_INVALID_ID,
            format,
            buf: va::VA_INVALID_ID,
            width: 0,
            height: 0,
            data_size: 0,
            num_planes: 0,
            pitches: [0; 3],
            offsets: [0; 3],
            num_palette_entries: 0,
            entry_bytes: 0,
            component_order: [0; 4],
            va_reserved: [0; 4],
        };
        // SAFETY: plain out-parameters.
        unsafe {
            check(
                va::vaCreateImage(
                    self.display.raw(),
                    &mut format,
                    self.width as i32,
                    self.height as i32,
                    &mut id,
                ),
                "vaCreateImage",
            )?;
        }
        Ok(VaImage {
            display: self.display.clone(),
            id,
            mapped: false,
        })
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        // SAFETY: the id came from vaCreateSurfaces on this display.
        unsafe {
            va::vaDestroySurfaces(self.display.raw(), &mut self.id, 1);
        }
    }
}

fn int_attrib(type_: va::VASurfaceAttribType, value: i32) -> va::VASurfaceAttrib {
    va::VASurfaceAttrib {
        type_,
        flags: va::VA_SURFACE_ATTRIB_SETTABLE,
        value: va::VAGenericValue {
            type_: va::VAGenericValueTypeInteger,
            value: va::_VAGenericValue__bindgen_ty_1 { i: value },
        },
    }
}

/// A host-visible copy of a surface's pixels (`vaCreateImage`).
struct VaImage {
    display: Arc<Display>,
    id: va::VAImage,
    mapped: bool,
}

impl VaImage {
    /// # Safety
    /// The pointer is valid until `unmap` or drop.
    unsafe fn map(&mut self) -> Result<*mut u8> {
        let mut ptr: *mut std::ffi::c_void = std::ptr::null_mut();
        // SAFETY: valid buffer id from vaCreateImage.
        unsafe {
            check(
                va::vaMapBuffer(self.display.raw(), self.id.buf, &mut ptr),
                "vaMapBuffer",
            )?;
        }
        self.mapped = true;
        Ok(ptr.cast())
    }

    fn unmap(&mut self) {
        if self.mapped {
            // SAFETY: mapped above.
            unsafe {
                va::vaUnmapBuffer(self.display.raw(), self.id.buf);
            }
            self.mapped = false;
        }
    }
}

impl Drop for VaImage {
    fn drop(&mut self) {
        self.unmap();
        // SAFETY: the image came from vaCreateImage on this display.
        unsafe {
            va::vaDestroyImage(self.display.raw(), self.id.image_id);
        }
    }
}
