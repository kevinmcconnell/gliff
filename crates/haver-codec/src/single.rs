//! Single420: one 4:2:0 H.264 stream, the `--low-bandwidth` fallback. Chroma is
//! subsampled on encode and duplicated back on decode, so colour resolution is
//! halved. This is never the quality baseline; it exists to save bandwidth.

use std::rc::Rc;
use std::sync::Arc;

use cros_codecs::libva::Display;
use haver_proto::chroma::{nv12_to_yuv444, yuv444_to_nv12, Yuv444};

use crate::frame::{FrameAllocator, Nv12Frame};
use crate::h264::{EncoderSettings, H264Decoder, H264Encoder};
use crate::Result;

pub struct SingleEncoder {
    enc: H264Encoder,
    pending_keyframe: bool,
}

impl SingleEncoder {
    pub fn new(display: Rc<Display>, settings: EncoderSettings) -> Result<Self> {
        Ok(Self {
            enc: H264Encoder::new(display, settings)?,
            pending_keyframe: false,
        })
    }

    pub fn request_keyframe(&mut self) {
        self.pending_keyframe = true;
    }

    /// Encode one frame; returns (main bytes, keyframe).
    pub fn encode(
        &mut self,
        src: &Yuv444,
        timestamp: u64,
        force_keyframe: bool,
    ) -> Result<(Vec<u8>, bool)> {
        let force = force_keyframe || std::mem::take(&mut self.pending_keyframe);
        let m = yuv444_to_nv12(src);
        let p = self
            .enc
            .encode_planes(&m.y, m.width, &m.uv, m.width, timestamp, force)?;
        Ok((p.data, p.keyframe))
    }
}

pub struct SingleDecoder {
    dec: H264Decoder,
    width: usize,
    height: usize,
}

impl SingleDecoder {
    pub fn new(display: Rc<Display>, width: usize, height: usize) -> Result<Self> {
        let alloc = FrameAllocator::open(&crate::vaapi::render_node(None))?;
        Ok(Self {
            dec: H264Decoder::new(display, alloc, 2)?,
            width,
            height,
        })
    }

    /// Decode and return the raw NV12 dmabuf frame (for the GPU display path).
    pub fn decode_frame(&mut self, timestamp: u64, main: &[u8]) -> Result<Option<Arc<Nv12Frame>>> {
        Ok(self
            .dec
            .decode(timestamp, main)?
            .into_iter()
            .next()
            .map(|f| f.frame))
    }

    pub fn dims(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    pub fn decode(&mut self, timestamp: u64, main: &[u8]) -> Result<Option<Yuv444>> {
        match self.decode_frame(timestamp, main)? {
            Some(frame) => Ok(Some(nv12_to_yuv444(
                &frame.read_nv12(self.width, self.height)?,
            ))),
            None => Ok(None),
        }
    }
}
