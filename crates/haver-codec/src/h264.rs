//! H.264 encode and decode wrappers over cros-codecs' stateless VA-API path.

use std::rc::Rc;
use std::sync::Arc;

use cros_codecs::backend::vaapi::decoder::VaapiBackend as DecBackend;
use cros_codecs::backend::vaapi::encoder::VaapiBackend as EncBackend;
use cros_codecs::codec::h264::parser::{Level, Profile};
use cros_codecs::decoder::stateless::h264::H264;
use cros_codecs::decoder::stateless::{DecodeError, StatelessDecoder, StatelessVideoDecoder};
use cros_codecs::decoder::{DecodedHandle, DecoderEvent};
use cros_codecs::encoder::h264::EncoderConfig;
use cros_codecs::encoder::stateless::h264::StatelessEncoder;
use cros_codecs::encoder::{FrameMetadata, PredictionStructure, RateControl, Tunings, VideoEncoder};
use cros_codecs::libva::{Display, Surface};
use cros_codecs::video_frame::generic_dma_video_frame::GenericDmaVideoFrame;
use cros_codecs::{BlockingMode, Resolution};

use crate::frame::{align_up, nv12, FrameAllocator, FramePool, Nv12Frame};
use crate::{annexb, Error, Result};

#[derive(Debug, Clone)]
pub struct EncoderSettings {
    pub width: u32,
    pub height: u32,
    pub qp: u32,
    pub framerate: u32,
    pub low_power: bool,
}

impl EncoderSettings {
    pub fn coded_width(&self) -> u32 {
        align_up(self.width, 16)
    }

    pub fn coded_height(&self) -> u32 {
        align_up(self.height, 16)
    }
}

#[derive(Debug, Clone)]
pub struct EncodedPacket {
    pub timestamp: u64,
    pub keyframe: bool,
    pub data: Vec<u8>,
}

type Encoder = StatelessEncoder<Nv12Frame, EncBackend<GenericDmaVideoFrame, Surface<GenericDmaVideoFrame>>>;

pub struct H264Encoder {
    inner: Encoder,
    settings: EncoderSettings,
    parameter_sets: Vec<u8>,
}

impl H264Encoder {
    pub fn new(display: Rc<Display>, settings: EncoderSettings) -> Result<Self> {
        let config = EncoderConfig {
            resolution: Resolution { width: settings.width, height: settings.height },
            profile: Profile::High,
            level: Level::L5_1,
            pred_structure: PredictionStructure::LowDelay { limit: 60 * 60 * 24 },
            initial_tunings: Tunings {
                rate_control: RateControl::ConstantQuality(settings.qp),
                framerate: settings.framerate,
                min_quality: 1,
                max_quality: 51,
            },
        };
        let coded = Resolution { width: settings.coded_width(), height: settings.coded_height() };
        let inner = Encoder::new_vaapi(display, config, nv12(), coded, settings.low_power, BlockingMode::Blocking)
            .map_err(|e| Error::Encode(e.to_string()))?;
        Ok(Self { inner, settings, parameter_sets: Vec::new() })
    }

    pub fn settings(&self) -> &EncoderSettings {
        &self.settings
    }

    /// SPS+PPS seen on the last keyframe, Annex B framed.
    pub fn parameter_sets(&self) -> &[u8] {
        &self.parameter_sets
    }

    pub fn set_qp(&mut self, qp: u32) -> Result<()> {
        self.settings.qp = qp;
        self.inner
            .tune(Tunings {
                rate_control: RateControl::ConstantQuality(qp),
                framerate: self.settings.framerate,
                min_quality: 1,
                max_quality: 51,
            })
            .map_err(|e| Error::Encode(e.to_string()))
    }

    /// Encode one frame. The frame must come from a pool of the coded size.
    pub fn encode(&mut self, frame: Nv12Frame, timestamp: u64, force_keyframe: bool) -> Result<EncodedPacket> {
        let layout = frame.layout();
        let meta = FrameMetadata { timestamp, layout, force_keyframe };
        self.inner.encode(meta, frame).map_err(|e| Error::Encode(e.to_string()))?;
        let mut packets = Vec::new();
        while let Some(buf) = self.inner.poll().map_err(|e| Error::Encode(e.to_string()))? {
            packets.push(buf);
        }
        let mut data = Vec::new();
        let mut timestamp_out = timestamp;
        for p in packets {
            timestamp_out = p.metadata.timestamp;
            data.extend_from_slice(&p.bitstream);
        }
        if data.is_empty() {
            return Err(Error::Encode("encoder produced no output".into()));
        }
        let keyframe = annexb::contains_idr(&data);
        if keyframe {
            let ps = annexb::parameter_sets(&data);
            if !ps.is_empty() {
                self.parameter_sets = ps;
            }
        }
        data.extend_from_slice(&annexb::AUD);
        Ok(EncodedPacket { timestamp: timestamp_out, keyframe, data })
    }
}

#[derive(Debug)]
pub struct DecodedFrame {
    pub timestamp: u64,
    pub width: u32,
    pub height: u32,
    pub frame: Arc<Nv12Frame>,
}

type Decoder = StatelessDecoder<H264, DecBackend<Nv12Frame>>;

pub struct H264Decoder {
    inner: Decoder,
    allocator: FrameAllocator,
    pool: Option<FramePool>,
    extra_frames: usize,
}

impl H264Decoder {
    /// `extra_frames` is how many decoded frames the caller may hold at once on
    /// top of what the decoder needs for references.
    pub fn new(display: Rc<Display>, allocator: FrameAllocator, extra_frames: usize) -> Result<Self> {
        let inner = Decoder::new_vaapi(display, BlockingMode::NonBlocking).map_err(|e| Error::Decode(e.to_string()))?;
        Ok(Self { inner, allocator, pool: None, extra_frames })
    }

    pub fn pool(&self) -> Option<&FramePool> {
        self.pool.as_ref()
    }

    /// Feed one access unit (Annex B). Returns frames that became ready.
    pub fn decode(&mut self, timestamp: u64, access_unit: &[u8]) -> Result<Vec<DecodedFrame>> {
        let mut ready = Vec::new();
        let mut offset = 0;
        let mut stalls = 0;
        while offset < access_unit.len() {
            let pool = &self.pool;
            let mut alloc = || pool.as_ref().and_then(FramePool::alloc);
            match self.inner.decode(timestamp, &access_unit[offset..], &mut alloc) {
                Ok(consumed) => {
                    offset += consumed;
                    stalls = 0;
                }
                Err(DecodeError::CheckEvents) | Err(DecodeError::NotEnoughOutputBuffers(_)) => {
                    self.drain_events(&mut ready)?;
                    stalls += 1;
                    if stalls > 8 {
                        return Err(Error::Decode("decoder stalled waiting for output buffers".into()));
                    }
                }
                Err(e) => return Err(Error::Decode(e.to_string())),
            }
        }
        self.drain_events(&mut ready)?;
        Ok(ready)
    }

    /// Drop all state so the next IDR starts a fresh stream.
    pub fn flush(&mut self) -> Result<()> {
        self.inner.flush().map_err(|e| Error::Decode(e.to_string()))?;
        let mut sink = Vec::new();
        self.drain_events(&mut sink)?;
        Ok(())
    }

    fn drain_events(&mut self, ready: &mut Vec<DecodedFrame>) -> Result<()> {
        while let Some(event) = self.inner.next_event() {
            match event {
                DecoderEvent::FrameReady(handle) => {
                    handle.sync().map_err(|e| Error::Decode(e.to_string()))?;
                    let res = handle.display_resolution();
                    ready.push(DecodedFrame {
                        timestamp: handle.timestamp(),
                        width: res.width,
                        height: res.height,
                        frame: handle.video_frame(),
                    });
                }
                DecoderEvent::FormatChanged => {
                    let info = self
                        .inner
                        .stream_info()
                        .ok_or_else(|| Error::Decode("format changed without stream info".into()))?
                        .clone();
                    let count = info.min_num_frames + self.extra_frames + 1;
                    tracing::info!(
                        coded = ?info.coded_resolution,
                        display = ?info.display_resolution,
                        frames = count,
                        "decoder format changed"
                    );
                    self.pool = Some(FramePool::new(
                        &self.allocator,
                        info.coded_resolution.width,
                        info.coded_resolution.height,
                        count,
                    )?);
                }
            }
        }
        Ok(())
    }
}
