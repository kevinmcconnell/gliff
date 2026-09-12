//! H.264 encode and decode wrappers over cros-codecs' stateless VA-API path.

use std::borrow::Borrow;
use std::rc::Rc;
use std::sync::Arc;

use cros_codecs::backend::vaapi::decoder::VaapiBackend as DecBackend;
use cros_codecs::backend::vaapi::encoder::VaapiBackend as EncBackend;
use cros_codecs::backend::vaapi::surface_pool::{PooledVaSurface, VaSurfacePool};
use cros_codecs::codec::h264::parser::{Level, Profile};
use cros_codecs::decoder::stateless::h264::H264;
use cros_codecs::decoder::stateless::{DecodeError, StatelessDecoder, StatelessVideoDecoder};
use cros_codecs::decoder::FramePool as _;
use cros_codecs::decoder::{DecodedHandle, DecoderEvent};
use cros_codecs::encoder::h264::EncoderConfig;
use cros_codecs::encoder::stateless::h264::StatelessEncoder;
use cros_codecs::encoder::{
    FrameMetadata, PredictionStructure, RateControl, Tunings, VideoEncoder,
};
use cros_codecs::libva::{
    Display, Image, UsageHint, VAProfile, VA_FOURCC_NV12, VA_RT_FORMAT_YUV420,
};
use cros_codecs::{BlockingMode, FrameLayout, PlaneLayout, Resolution};

use crate::frame::{align_up, nv12, FrameAllocator, FramePool, Nv12Frame};
use crate::{annexb, Error, Result};

#[derive(Debug, Clone)]
pub struct EncoderSettings {
    pub width: u32,
    pub height: u32,
    /// Target bitrate in bits per second (CBR).
    pub bitrate: u32,
    pub framerate: u32,
    pub low_power: bool,
}

impl EncoderSettings {
    /// A rough default bitrate for a desktop stream at this size and rate.
    pub fn default_bitrate(width: u32, height: u32, framerate: u32) -> u32 {
        // ~0.1 bits per pixel per frame keeps text crisp on a LAN.
        ((width as u64 * height as u64 * framerate as u64) / 10).min(80_000_000) as u32
    }

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

type Encoder = StatelessEncoder<PooledVaSurface<()>, EncBackend<(), PooledVaSurface<()>>>;

pub struct H264Encoder {
    inner: Encoder,
    pool: VaSurfacePool<()>,
    image_format: cros_codecs::libva::VAImageFormat,
    settings: EncoderSettings,
    parameter_sets: Vec<u8>,
    frames_encoded: u64,
}

impl H264Encoder {
    pub fn new(display: Rc<Display>, settings: EncoderSettings) -> Result<Self> {
        let config = EncoderConfig {
            resolution: Resolution {
                width: settings.width,
                height: settings.height,
            },
            profile: Profile::High,
            level: Level::L5_1,
            pred_structure: PredictionStructure::LowDelay { limit: 32768 },
            initial_tunings: Tunings {
                rate_control: RateControl::ConstantBitrate(settings.bitrate as u64),
                framerate: settings.framerate,
                min_quality: 1,
                max_quality: 51,
            },
        };
        let coded = Resolution {
            width: settings.coded_width(),
            height: settings.coded_height(),
        };
        // The AMD/radeonsi VA-API encoder does not read a linear external
        // dmabuf as its input surface, so input frames are uploaded into
        // driver-owned surfaces with `vaPutImage`. This is the path
        // cros-codecs' own encoder tests exercise.
        let backend = EncBackend::<(), PooledVaSurface<()>>::new(
            display.clone(),
            VAProfile::VAProfileH264High,
            nv12(),
            coded,
            cros_codecs::libva::VA_RC_CBR,
            settings.low_power,
        )
        .map_err(|e| Error::Encode(e.to_string()))?;
        let inner = Encoder::new_h264(backend, config, BlockingMode::Blocking)
            .map_err(|e| Error::Encode(e.to_string()))?;
        let mut pool = VaSurfacePool::<()>::new(
            display.clone(),
            VA_RT_FORMAT_YUV420,
            Some(UsageHint::USAGE_HINT_ENCODER),
            coded,
        );
        pool.add_frames(vec![(); 8])
            .map_err(|e| Error::Encode(e.to_string()))?;
        let image_format = display
            .query_image_formats()
            .map_err(|e| Error::Encode(e.to_string()))?
            .into_iter()
            .find(|f| f.fourcc == VA_FOURCC_NV12)
            .ok_or_else(|| Error::Encode("driver has no NV12 image format".into()))?;
        Ok(Self {
            inner,
            pool,
            image_format,
            settings,
            parameter_sets: Vec::new(),
            frames_encoded: 0,
        })
    }

    fn take_surface(&mut self) -> Result<PooledVaSurface<()>> {
        if let Some(s) = self.pool.get_surface() {
            return Ok(s);
        }
        self.pool
            .add_frames(vec![(); 4])
            .map_err(|e| Error::Encode(e.to_string()))?;
        self.pool
            .get_surface()
            .ok_or_else(|| Error::Encode("surface pool exhausted".into()))
    }

    fn upload(
        &self,
        surface: &PooledVaSurface<()>,
        y: &[u8],
        y_stride: usize,
        uv: &[u8],
        uv_stride: usize,
    ) -> Result<()> {
        let (w, h) = (self.settings.width as usize, self.settings.height as usize);
        let (cw, ch) = (self.settings.coded_width(), self.settings.coded_height());
        let surf: &cros_codecs::libva::Surface<()> = surface.borrow();
        let mut image = Image::create_from(
            surf,
            self.image_format,
            (cw, ch),
            (self.settings.width, self.settings.height),
        )
        .map_err(|e| Error::Encode(format!("create image: {e}")))?;
        let va = *image.image();
        let dst = image.as_mut();
        let (yo, uvo) = (va.offsets[0] as usize, va.offsets[1] as usize);
        let (yp, uvp) = (va.pitches[0] as usize, va.pitches[1] as usize);
        for row in 0..h {
            dst[yo + row * yp..yo + row * yp + w]
                .copy_from_slice(&y[row * y_stride..row * y_stride + w]);
        }
        for row in 0..h / 2 {
            dst[uvo + row * uvp..uvo + row * uvp + w]
                .copy_from_slice(&uv[row * uv_stride..row * uv_stride + w]);
        }
        drop(image);
        surf.sync()
            .map_err(|e| Error::Encode(format!("surface sync: {e}")))?;
        Ok(())
    }

    pub fn settings(&self) -> &EncoderSettings {
        &self.settings
    }

    /// SPS+PPS seen on the last keyframe, Annex B framed.
    pub fn parameter_sets(&self) -> &[u8] {
        &self.parameter_sets
    }

    pub fn set_bitrate(&mut self, bitrate: u32) -> Result<()> {
        self.settings.bitrate = bitrate;
        self.inner
            .tune(Tunings {
                rate_control: RateControl::ConstantBitrate(bitrate as u64),
                framerate: self.settings.framerate,
                min_quality: 1,
                max_quality: 51,
            })
            .map_err(|e| Error::Encode(e.to_string()))
    }

    /// Encode one NV12 frame supplied as separate Y and UV plane slices.
    pub fn encode_planes(
        &mut self,
        y: &[u8],
        y_stride: usize,
        uv: &[u8],
        uv_stride: usize,
        timestamp: u64,
        force_keyframe: bool,
    ) -> Result<EncodedPacket> {
        let handle = self.take_surface()?;
        self.upload(&handle, y, y_stride, uv, uv_stride)?;
        self.encode_surface(handle, timestamp, force_keyframe)
    }

    /// Take an unused input surface from the pool so a caller can fill it
    /// directly (for example by rendering into its exported dmabuf on the GPU),
    /// then hand it back to [`encode_surface`].
    pub fn acquire_surface(&mut self) -> Result<PooledVaSurface<()>> {
        self.take_surface()
    }

    pub fn coded_size(&self) -> (u32, u32) {
        (self.settings.coded_width(), self.settings.coded_height())
    }

    /// Encode a surface whose NV12 content is already in place.
    pub fn encode_surface(
        &mut self,
        handle: PooledVaSurface<()>,
        timestamp: u64,
        force_keyframe: bool,
    ) -> Result<EncodedPacket> {
        let layout = FrameLayout {
            format: (nv12(), 0),
            size: Resolution {
                width: self.settings.coded_width(),
                height: self.settings.coded_height(),
            },
            planes: vec![
                PlaneLayout {
                    buffer_index: 0,
                    offset: 0,
                    stride: self.settings.coded_width() as usize,
                },
                PlaneLayout {
                    buffer_index: 0,
                    offset: (self.settings.coded_width() * self.settings.coded_height()) as usize,
                    stride: self.settings.coded_width() as usize,
                },
            ],
        };
        let meta = FrameMetadata {
            timestamp,
            layout,
            force_keyframe,
        };
        self.inner
            .encode(meta, handle)
            .map_err(|e| Error::Encode(e.to_string()))?;
        let mut packets = Vec::new();
        while let Some(buf) = self
            .inner
            .poll()
            .map_err(|e| Error::Encode(e.to_string()))?
        {
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
        // cros-codecs makes the first frame and every forced frame an IDR.
        let keyframe = force_keyframe || self.frames_encoded == 0;
        self.frames_encoded += 1;
        // Quirk: on Mesa radeonsi the VA-API H.264 encoder returns each coded
        // slice with a zeroed NAL unit header byte, because cros-codecs does
        // not supply a packed slice header. Fill it: IDR slices are type 5,
        // referenced P slices type 1, both with nal_ref_idc = 3 here.
        let header = if keyframe { 0x65u8 } else { 0x61u8 };
        // Single forward pass: fix each zeroed header, then resume scanning
        // after it. Fixing the header restores the byte the encoder's
        // emulation-prevention assumed, so a later slice's RBSP cannot be
        // mistaken for a start code and clobbered.
        annexb::fix_zeroed_nal_headers(&mut data, header);
        let ps = annexb::parameter_sets(&data);
        if !ps.is_empty() {
            self.parameter_sets = ps;
        } else if keyframe && !self.parameter_sets.is_empty() {
            // cros-codecs only emits SPS/PPS on the very first IDR, so a later
            // forced keyframe would not be self-describing. Prepend them so a
            // reconnecting or recovering decoder can start from any keyframe.
            let mut with_ps = self.parameter_sets.clone();
            with_ps.extend_from_slice(&data);
            data = with_ps;
        }
        data.extend_from_slice(&annexb::AUD);
        Ok(EncodedPacket {
            timestamp: timestamp_out,
            keyframe,
            data,
        })
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
    pub fn new(
        display: Rc<Display>,
        allocator: FrameAllocator,
        extra_frames: usize,
    ) -> Result<Self> {
        let inner = Decoder::new_vaapi(display, BlockingMode::NonBlocking)
            .map_err(|e| Error::Decode(e.to_string()))?;
        Ok(Self {
            inner,
            allocator,
            pool: None,
            extra_frames,
        })
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
            if !annexb::has_start_code(&access_unit[offset..]) {
                break;
            }
            let pool = &self.pool;
            let mut alloc = || pool.as_ref().and_then(FramePool::alloc);
            match self
                .inner
                .decode(timestamp, &access_unit[offset..], &mut alloc)
            {
                Ok(consumed) => {
                    offset += consumed;
                    stalls = 0;
                }
                Err(DecodeError::CheckEvents) | Err(DecodeError::NotEnoughOutputBuffers(_)) => {
                    self.drain_events(&mut ready)?;
                    stalls += 1;
                    if stalls > 8 {
                        return Err(Error::Decode(
                            "decoder stalled waiting for output buffers".into(),
                        ));
                    }
                }
                Err(e) => {
                    let tail: Vec<String> = access_unit[offset..]
                        .iter()
                        .take(8)
                        .map(|b| format!("{b:02x}"))
                        .collect();
                    tracing::debug!(
                        offset,
                        len = access_unit.len(),
                        tail = tail.join(" "),
                        "decode error"
                    );
                    return Err(Error::Decode(e.to_string()));
                }
            }
        }
        self.drain_events(&mut ready)?;
        Ok(ready)
    }

    /// Drop all state so the next IDR starts a fresh stream.
    pub fn flush(&mut self) -> Result<()> {
        self.inner
            .flush()
            .map_err(|e| Error::Decode(e.to_string()))?;
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
