//! Dual420: full 4:4:4 over two 4:2:0 H.264 streams (the AVC444 technique).
//!
//! The main stream carries luma plus the even-position chroma; the auxiliary
//! stream carries the chroma the main stream dropped. Both run through their
//! own encoder with identical settings and keyframe cadence.

use std::rc::Rc;

use cros_codecs::libva::Display;
use haver_proto::chroma::{recombine_yuv444, split_yuv444, Nv12, Yuv444};

use crate::h264::{EncoderSettings, H264Decoder, H264Encoder};
use crate::{frame::FrameAllocator, Result};

/// Two encoded streams for one 4:4:4 frame.
#[derive(Debug, Clone)]
pub struct DualPacket {
    pub timestamp: u64,
    pub keyframe: bool,
    pub main: Vec<u8>,
    pub aux: Vec<u8>,
}

pub struct DualEncoder {
    main: H264Encoder,
    aux: H264Encoder,
}

impl DualEncoder {
    pub fn new(display: Rc<Display>, settings: EncoderSettings) -> Result<Self> {
        let main = H264Encoder::new(display.clone(), settings.clone())?;
        // The auxiliary stream is pure chroma; keep its settings identical so
        // keyframes line up. Its QP/bitrate can be tuned later.
        let aux = H264Encoder::new(display, settings)?;
        Ok(Self { main, aux })
    }

    pub fn main_extradata(&self) -> &[u8] {
        self.main.parameter_sets()
    }

    pub fn aux_extradata(&self) -> &[u8] {
        self.aux.parameter_sets()
    }

    pub fn request_keyframe(&mut self) {
        // Both streams force an IDR on the next frame via the encode flag.
    }

    /// Encode one 4:4:4 frame into two H.264 access units.
    pub fn encode(&mut self, src: &Yuv444, timestamp: u64, force_keyframe: bool) -> Result<DualPacket> {
        let (m, a) = split_yuv444(src);
        let mp = self.main.encode_planes(&m.y, m.width, &m.uv, m.width, timestamp, force_keyframe)?;
        let ap = self.aux.encode_planes(&a.y, a.width, &a.uv, a.width, timestamp, force_keyframe)?;
        Ok(DualPacket { timestamp, keyframe: mp.keyframe, main: mp.data, aux: ap.data })
    }
}

pub struct DualDecoder {
    main: H264Decoder,
    aux: H264Decoder,
    width: usize,
    height: usize,
}

impl DualDecoder {
    pub fn new(display: Rc<Display>, width: usize, height: usize) -> Result<Self> {
        let a = FrameAllocator::open(&crate::vaapi::render_node(None))?;
        let b = FrameAllocator::open(&crate::vaapi::render_node(None))?;
        Ok(Self {
            main: H264Decoder::new(display.clone(), a, 2)?,
            aux: H264Decoder::new(display, b, 2)?,
            width,
            height,
        })
    }

    /// Decode one dual access unit into a 4:4:4 frame, if both streams produced one.
    pub fn decode(&mut self, timestamp: u64, main: &[u8], aux: &[u8]) -> Result<Option<Yuv444>> {
        let m = self.main.decode(timestamp, main)?;
        let a = self.aux.decode(timestamp, aux)?;
        let (Some(mf), Some(af)) = (m.into_iter().next(), a.into_iter().next()) else {
            return Ok(None);
        };
        let main_nv12 = read_nv12(&mf, self.width, self.height)?;
        let aux_nv12 = read_nv12(&af, self.width, self.height)?;
        Ok(Some(recombine_yuv444(&main_nv12, &aux_nv12)))
    }
}

fn read_nv12(frame: &crate::h264::DecodedFrame, width: usize, height: usize) -> Result<Nv12> {
    let mut out = Nv12::new(width, height);
    frame.frame.with_planes(|y, uv, py, puv| {
        for row in 0..height {
            out.y[row * width..row * width + width].copy_from_slice(&y[row * py..row * py + width]);
        }
        for row in 0..height / 2 {
            out.uv[row * width..row * width + width].copy_from_slice(&uv[row * puv..row * puv + width]);
        }
    })?;
    Ok(out)
}
