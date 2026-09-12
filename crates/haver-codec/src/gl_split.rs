//! Dual420 with the 4:4:4 split done on the GPU.
//!
//! Instead of the CPU colour + split stage ([`crate::color`] plus
//! [`haver_proto::chroma::split_yuv444`]), this renders the captured BGRA
//! dmabuf into the two encoder input surfaces with a GL shader, then encodes
//! those surfaces. It sidesteps the AMD/radeonsi limit that the encoder cannot
//! read an external dmabuf as input: the surfaces are the encoder's own, and
//! GL writes into them through their exported dmabufs.

use std::os::fd::AsFd;
use std::path::Path;
use std::rc::Rc;

use cros_codecs::libva::{Display, DrmPrimeSurfaceDescriptor};
use drm_fourcc::DrmFourcc;
use haver_gl::{DmabufPlane, Headless};

use crate::dual::DualPacket;
use crate::h264::{EncoderSettings, H264Encoder};
use crate::{Error, Result};

pub struct GlDualEncoder {
    headless: Headless,
    main: H264Encoder,
    aux: H264Encoder,
    width: u32,
    height: u32,
    pending_keyframe: bool,
}

impl GlDualEncoder {
    pub fn new(display: Rc<Display>, render_node: &Path, settings: EncoderSettings) -> Result<Self> {
        let headless = Headless::new(render_node).map_err(|e| Error::Encode(format!("headless GL: {e}")))?;
        let width = settings.width;
        let height = settings.height;
        let main = H264Encoder::new(display.clone(), settings.clone())?;
        let aux = H264Encoder::new(display, settings)?;
        Ok(Self { headless, main, aux, width, height, pending_keyframe: false })
    }

    pub fn main_extradata(&self) -> &[u8] {
        self.main.parameter_sets()
    }

    pub fn aux_extradata(&self) -> &[u8] {
        self.aux.parameter_sets()
    }

    pub fn request_keyframe(&mut self) {
        self.pending_keyframe = true;
    }

    /// Encode one captured BGRA/BGRX frame, splitting 4:4:4 on the GPU.
    pub fn encode(&mut self, input: &DmabufPlane, timestamp: u64, force_keyframe: bool) -> Result<DualPacket> {
        let force = force_keyframe || std::mem::take(&mut self.pending_keyframe);
        let (w, h) = (self.width, self.height);

        let main_handle = self.main.acquire_surface()?;
        let aux_handle = self.aux.acquire_surface()?;

        {
            use std::borrow::Borrow;
            let main_surf: &cros_codecs::libva::Surface<()> = main_handle.borrow();
            let aux_surf: &cros_codecs::libva::Surface<()> = aux_handle.borrow();
            let main_desc = main_surf.export_prime().map_err(|e| Error::Encode(format!("export main: {e}")))?;
            let aux_desc = aux_surf.export_prime().map_err(|e| Error::Encode(format!("export aux: {e}")))?;
            let (my, muv) = nv12_planes(&main_desc);
            let (ay, auv) = nv12_planes(&aux_desc);
            self.headless
                .split_dual(input, w, h, &my, &muv, &ay, &auv)
                .map_err(|e| Error::Encode(format!("gl split: {e}")))?;
            // Descriptors (and their dmabuf fds) drop here, before the encoder
            // reads the surfaces.
        }

        let mp = self.main.encode_surface(main_handle, timestamp, force)?;
        let ap = self.aux.encode_surface(aux_handle, timestamp, force)?;
        Ok(DualPacket { timestamp, keyframe: mp.keyframe, main: mp.data, aux: ap.data })
    }
}

/// The Y (R8) and UV (GR88) dmabuf planes of an exported NV12 surface. Both
/// planes live in the one object of a composed-layer export and share its fd.
pub fn nv12_planes(desc: &DrmPrimeSurfaceDescriptor) -> (DmabufPlane<'_>, DmabufPlane<'_>) {
    let obj = &desc.objects[0];
    let layer = &desc.layers[0];
    let y = DmabufPlane {
        fd: obj.fd.as_fd(),
        width: desc.width,
        height: desc.height,
        offset: layer.offset[0],
        stride: layer.pitch[0],
        fourcc: DrmFourcc::R8,
        modifier: obj.drm_format_modifier,
    };
    let uv = DmabufPlane {
        fd: obj.fd.as_fd(),
        width: desc.width / 2,
        height: desc.height / 2,
        offset: layer.offset[1],
        stride: layer.pitch[1],
        fourcc: DrmFourcc::Gr88,
        modifier: obj.drm_format_modifier,
    };
    (y, uv)
}
