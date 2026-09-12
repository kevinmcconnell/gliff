//! Dual420: full 4:4:4 over two 4:2:0 H.264 streams (the AVC444 technique).
//!
//! The main stream carries luma plus the even-position chroma; the auxiliary
//! stream carries the chroma the main stream dropped. Both run through their
//! own encoder with identical settings and keyframe cadence.

use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;

use cros_codecs::libva::Display;
use haver_proto::chroma::{recombine_yuv444, split_yuv444, Yuv444};

use crate::frame::Nv12Frame;
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
    pending_keyframe: bool,
}

impl DualEncoder {
    pub fn new(display: Rc<Display>, settings: EncoderSettings) -> Result<Self> {
        let main = H264Encoder::new(display.clone(), settings.clone())?;
        // The auxiliary stream is pure chroma; keep its settings identical so
        // keyframes line up. Its QP/bitrate can be tuned later.
        let aux = H264Encoder::new(display, settings)?;
        Ok(Self {
            main,
            aux,
            pending_keyframe: false,
        })
    }

    pub fn main_extradata(&self) -> &[u8] {
        self.main.parameter_sets()
    }

    pub fn aux_extradata(&self) -> &[u8] {
        self.aux.parameter_sets()
    }

    /// Force both streams to emit an IDR on the next encoded frame.
    pub fn request_keyframe(&mut self) {
        self.pending_keyframe = true;
    }

    /// Encode one 4:4:4 frame into two H.264 access units.
    pub fn encode(
        &mut self,
        src: &Yuv444,
        timestamp: u64,
        force_keyframe: bool,
    ) -> Result<DualPacket> {
        let force = force_keyframe || std::mem::take(&mut self.pending_keyframe);
        let (m, a) = split_yuv444(src);
        let mp = self
            .main
            .encode_planes(&m.y, m.width, &m.uv, m.width, timestamp, force)?;
        let ap = self
            .aux
            .encode_planes(&a.y, a.width, &a.uv, a.width, timestamp, force)?;
        Ok(DualPacket {
            timestamp,
            keyframe: mp.keyframe,
            main: mp.data,
            aux: ap.data,
        })
    }
}

/// A decoded pair of NV12 dmabuf frames (main + auxiliary) plus display size.
pub struct DecodedPair {
    pub main: Arc<Nv12Frame>,
    pub aux: Arc<Nv12Frame>,
    pub width: usize,
    pub height: usize,
}

pub struct DualDecoder {
    main: H264Decoder,
    aux: H264Decoder,
    width: usize,
    height: usize,
    // Ready main/aux frames not yet paired, keyed by their timestamp. The two
    // decoders are independent and may return a frame on different calls, so we
    // pair by timestamp rather than by arrival order. We keep the dmabuf frames
    // themselves (not CPU copies) so the GPU path stays zero-copy.
    main_ready: BTreeMap<u64, Arc<Nv12Frame>>,
    aux_ready: BTreeMap<u64, Arc<Nv12Frame>>,
}

impl DualDecoder {
    pub fn new(display: Rc<Display>, width: usize, height: usize) -> Result<Self> {
        let a = FrameAllocator::open(&crate::vaapi::render_node(None))?;
        let b = FrameAllocator::open(&crate::vaapi::render_node(None))?;
        Ok(Self {
            main: H264Decoder::new(display.clone(), a, 3)?,
            aux: H264Decoder::new(display, b, 3)?,
            width,
            height,
            main_ready: BTreeMap::new(),
            aux_ready: BTreeMap::new(),
        })
    }

    /// Decode one dual access unit and pair by timestamp. Returns the two NV12
    /// dmabuf frames once both streams have produced a matching timestamp.
    pub fn decode_pair(
        &mut self,
        timestamp: u64,
        main: &[u8],
        aux: &[u8],
    ) -> Result<Option<DecodedPair>> {
        for f in self.main.decode(timestamp, main)? {
            self.main_ready.insert(f.timestamp, f.frame);
        }
        for f in self.aux.decode(timestamp, aux)? {
            self.aux_ready.insert(f.timestamp, f.frame);
        }
        // If one stream stalls, its partner's map must not grow without bound
        // (each pinned frame holds a pooled dmabuf). Keep only the newest few.
        const MAX_UNPAIRED: usize = 4;
        while self.main_ready.len() > MAX_UNPAIRED {
            let oldest = *self.main_ready.keys().next().expect("non-empty");
            self.main_ready.remove(&oldest);
        }
        while self.aux_ready.len() > MAX_UNPAIRED {
            let oldest = *self.aux_ready.keys().next().expect("non-empty");
            self.aux_ready.remove(&oldest);
        }
        let ts = self
            .main_ready
            .keys()
            .find(|k| self.aux_ready.contains_key(k))
            .copied();
        if let Some(ts) = ts {
            // Drop any older unpaired frames; their partner was lost.
            self.main_ready.retain(|k, _| *k >= ts);
            self.aux_ready.retain(|k, _| *k >= ts);
            let main = self.main_ready.remove(&ts).expect("present");
            let aux = self.aux_ready.remove(&ts).expect("present");
            return Ok(Some(DecodedPair {
                main,
                aux,
                width: self.width,
                height: self.height,
            }));
        }
        Ok(None)
    }

    /// Decode and recombine into a 4:4:4 frame on the CPU (fallback display).
    pub fn decode(&mut self, timestamp: u64, main: &[u8], aux: &[u8]) -> Result<Option<Yuv444>> {
        match self.decode_pair(timestamp, main, aux)? {
            Some(pair) => {
                let m = pair.main.read_nv12(self.width, self.height)?;
                let a = pair.aux.read_nv12(self.width, self.height)?;
                Ok(Some(recombine_yuv444(&m, &a)))
            }
            None => Ok(None),
        }
    }
}
