//! One encoder and one decoder handle for whichever chroma mode a session
//! negotiated: full 4:4:4 over two streams (`Dual420`) or the single 4:2:0
//! stream of `--low-bandwidth` (`Single420`).

use std::rc::Rc;
use std::sync::Arc;

use cros_codecs::libva::Display;
use haver_proto::chroma::Yuv444;
use haver_proto::ChromaMode;

use crate::dual::{DualDecoder, DualEncoder};
use crate::frame::Nv12Frame;
use crate::h264::EncoderSettings;
use crate::single::{SingleDecoder, SingleEncoder};
use crate::Result;

/// The encoded bitstreams for one frame; `aux` is present only for `Dual420`.
pub struct EncodedFrame {
    pub main: Vec<u8>,
    pub aux: Option<Vec<u8>>,
    pub keyframe: bool,
}

#[allow(clippy::large_enum_variant)]
pub enum Encoder {
    Dual(DualEncoder),
    Single(SingleEncoder),
}

impl Encoder {
    pub fn new(
        display: Rc<Display>,
        settings: EncoderSettings,
        chroma: ChromaMode,
    ) -> Result<Self> {
        Ok(match chroma {
            ChromaMode::Single420 => Self::Single(SingleEncoder::new(display, settings)?),
            ChromaMode::Dual420 | ChromaMode::Native444 => {
                Self::Dual(DualEncoder::new(display, settings)?)
            }
        })
    }

    pub fn encode(
        &mut self,
        src: &Yuv444,
        timestamp: u64,
        force_keyframe: bool,
    ) -> Result<EncodedFrame> {
        Ok(match self {
            Self::Dual(d) => {
                let p = d.encode(src, timestamp, force_keyframe)?;
                EncodedFrame {
                    main: p.main,
                    aux: Some(p.aux),
                    keyframe: p.keyframe,
                }
            }
            Self::Single(s) => {
                let (main, keyframe) = s.encode(src, timestamp, force_keyframe)?;
                EncodedFrame {
                    main,
                    aux: None,
                    keyframe,
                }
            }
        })
    }
}

/// Decoded dmabuf planes for the GPU display path; `aux` is present only for
/// `Dual420`.
pub struct DecodedPlanes {
    pub width: usize,
    pub height: usize,
    pub main: Arc<Nv12Frame>,
    pub aux: Option<Arc<Nv12Frame>>,
}

#[allow(clippy::large_enum_variant)]
pub enum Decoder {
    Dual(DualDecoder),
    Single(SingleDecoder),
}

impl Decoder {
    pub fn new(
        display: Rc<Display>,
        chroma: ChromaMode,
        width: usize,
        height: usize,
    ) -> Result<Self> {
        Ok(match chroma {
            ChromaMode::Single420 => Self::Single(SingleDecoder::new(display, width, height)?),
            ChromaMode::Dual420 | ChromaMode::Native444 => {
                Self::Dual(DualDecoder::new(display, width, height)?)
            }
        })
    }

    /// Decode without leaving the GPU: the caller imports the planes as textures.
    pub fn decode_planes(
        &mut self,
        timestamp: u64,
        main: &[u8],
        aux: &[u8],
    ) -> Result<Option<DecodedPlanes>> {
        Ok(match self {
            Self::Dual(d) => d.decode_pair(timestamp, main, aux)?.map(|p| DecodedPlanes {
                width: p.width,
                height: p.height,
                main: p.main,
                aux: Some(p.aux),
            }),
            Self::Single(s) => {
                let (width, height) = s.dims();
                s.decode_frame(timestamp, main)?.map(|main| DecodedPlanes {
                    width,
                    height,
                    main,
                    aux: None,
                })
            }
        })
    }

    /// Decode and recombine to 4:4:4 on the CPU.
    pub fn decode(&mut self, timestamp: u64, main: &[u8], aux: &[u8]) -> Result<Option<Yuv444>> {
        match self {
            Self::Dual(d) => d.decode(timestamp, main, aux),
            Self::Single(s) => s.decode(timestamp, main),
        }
    }
}
