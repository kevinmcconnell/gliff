//! The two ends of the stream as one object each.
//!
//! Server: captured dmabuf -> (import) -> split compute -> H.264 encode x2.
//! Client: H.264 decode x2 -> recombine compute -> BGRA dmabuf for display.
//!
//! Both use `Dual420` (main + aux streams, full 4:4:4) or `Single420` (main
//! only). Compute and video work are ordered on the GPU with a timeline
//! semaphore; the CPU waits once per frame, for the encoded bytes or the
//! finished display image.

use std::collections::HashMap;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::Duration;

use ash::vk;

use crate::compute::{Recombine, Split};
use crate::decoder::H264Decoder;
use crate::device::{Commands, Gpu, Timeline};
use crate::encoder::{EncoderSettings, H264Encoder};
use crate::image::{DmabufPlane, ExportedDmabuf, HostBuffer, Image};
use crate::Result;

pub struct EncodedFrame {
    pub main: Vec<u8>,
    pub aux: Option<Vec<u8>>,
    pub keyframe: bool,
}

/// Server side: one encoder object per session.
pub struct Encoder {
    gpu: Arc<Gpu>,
    timeline: Timeline,
    compute: Commands,
    split: Split,
    main: H264Encoder,
    aux: Option<H264Encoder>,
    main_in: Image,
    aux_in: Option<Image>,
    /// Imported capture buffers, keyed by the caller's buffer id.
    imports: HashMap<u64, Image>,
    settings: EncoderSettings,
}

impl Encoder {
    /// The largest size the device encodes, as (width, height).
    pub fn max_size(gpu: &Gpu) -> Result<(u32, u32)> {
        H264Encoder::max_coded_extent(gpu)
    }

    pub fn new(gpu: &Arc<Gpu>, settings: EncoderSettings, dual: bool) -> Result<Self> {
        let main = H264Encoder::new(gpu, settings.clone())?;
        let aux = if dual {
            Some(H264Encoder::new(gpu, settings.clone())?)
        } else {
            None
        };
        let main_in = main.new_input()?;
        let aux_in = aux.as_ref().map(|a| a.new_input()).transpose()?;
        Ok(Self {
            gpu: gpu.clone(),
            timeline: Timeline::new(gpu)?,
            compute: Commands::new(gpu, gpu.families.compute, gpu.compute_queue)?,
            split: Split::new(gpu)?,
            main,
            aux,
            main_in,
            aux_in,
            imports: HashMap::new(),
            settings,
        })
    }

    pub fn settings(&self) -> &EncoderSettings {
        &self.settings
    }

    /// Change the target bitrate of both streams from the next frame on.
    pub fn set_bitrate(&mut self, bitrate: u32) {
        let (fps, vbv) = (self.settings.framerate, self.settings.vbv_ms);
        self.set_rate(bitrate, fps, vbv);
    }

    /// Change the target bitrate, the frame rate it is spread over, and the
    /// rate-control buffer of both streams from the next frame on.
    pub fn set_rate(&mut self, bitrate: u32, framerate: u32, vbv_ms: u32) {
        self.settings.bitrate = bitrate;
        self.settings.framerate = framerate;
        self.settings.vbv_ms = vbv_ms;
        self.main.set_rate(bitrate, framerate, vbv_ms);
        if let Some(a) = &mut self.aux {
            a.set_rate(bitrate, framerate, vbv_ms);
        }
    }

    /// Encode a captured dmabuf. `key` identifies the buffer so its import
    /// is reused across frames; pass a new key when the buffer changes.
    pub fn encode_dmabuf(
        &mut self,
        key: u64,
        plane: &DmabufPlane,
        force_keyframe: bool,
    ) -> Result<EncodedFrame> {
        if !self.imports.contains_key(&key) {
            // A new capture ring means the old buffers are gone.
            if self.imports.len() >= 8 {
                self.compute.wait()?;
                self.imports.clear();
            }
            self.imports
                .insert(key, Image::import_dmabuf(&self.gpu, plane)?);
        }
        let src = self.imports.remove(&key).expect("inserted above");
        let result = self.encode_image(&src, force_keyframe);
        self.imports.insert(key, src);
        result
    }

    /// Encode packed BGRA pixels (tests and the probe; one extra upload).
    pub fn encode_bgra(&mut self, bgra: &[u8], force_keyframe: bool) -> Result<EncodedFrame> {
        let (w, h) = (self.settings.width, self.settings.height);
        let staging = HostBuffer::new(
            &self.gpu,
            bgra.len(),
            vk::BufferUsageFlags::TRANSFER_SRC,
            None,
        )?;
        staging.write(0, bgra);
        let src = Image::bgra_upload(&self.gpu, w, h)?;
        self.compute
            .run(self.timeline.semaphore, None, None, true, |cmd| {
                src.transition(cmd, vk::ImageLayout::TRANSFER_DST_OPTIMAL);
                src.copy_rgba_from_buffer(cmd, &staging);
                Ok(())
            })?;
        self.encode_image(&src, force_keyframe)
    }

    fn encode_image(&mut self, src: &Image, force_keyframe: bool) -> Result<EncodedFrame> {
        let (w, h) = (self.settings.width, self.settings.height);
        let split_done = self.timeline.advance();
        let (split, main_in, aux_in) = (&self.split, &self.main_in, self.aux_in.as_ref());
        self.compute.run(
            self.timeline.semaphore,
            None,
            Some(split_done),
            false,
            |cmd| {
                src.transition(cmd, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
                main_in.transition(cmd, vk::ImageLayout::GENERAL);
                if let Some(a) = aux_in {
                    a.transition(cmd, vk::ImageLayout::GENERAL);
                }
                split.record(cmd, src, main_in, aux_in, w, h)?;
                main_in.memory_barrier(cmd);
                Ok(())
            },
        )?;
        // Submit both encodes, then wait: the main stream's readback overlaps
        // the aux encode on the GPU.
        let main = self.main.submit(
            &self.main_in,
            &self.timeline,
            Some(split_done),
            force_keyframe,
        )?;
        let aux = match (&mut self.aux, &self.aux_in) {
            (Some(enc), Some(input)) => {
                Some(enc.submit(input, &self.timeline, Some(split_done), force_keyframe)?)
            }
            _ => None,
        };
        let main = self.main.finish(main)?;
        let aux = match (&mut self.aux, aux) {
            (Some(enc), Some(pending)) => Some(enc.finish(pending)?),
            _ => None,
        };
        Ok(EncodedFrame {
            keyframe: main.keyframe,
            main: main.data,
            aux: aux.map(|a| a.data),
        })
    }
}

/// A finished display frame: a linear BGRX dmabuf the display side imports.
/// The fd is a fresh duplicate the receiver owns. Dropping the frame returns
/// its image to the decoder's ring, so keep it alive until the display side
/// has finished with the texture.
#[derive(Debug)]
pub struct DisplayFrame {
    pub fd: OwnedFd,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub offset: u32,
    pub fourcc: drm_fourcc::DrmFourcc,
    pub modifier: u64,
    /// Display pixels per stream pixel, see [`Decoder::set_zoom`].
    pub zoom: u32,
    ring: u64,
    index: usize,
    release: Sender<(u64, usize)>,
}

impl Drop for DisplayFrame {
    fn drop(&mut self) {
        let _ = self.release.send((self.ring, self.index));
    }
}

/// Display images kept by the client decoder: one being written, one on
/// screen, one in transit between the two.
const DISPLAY_RING: usize = 3;

/// Client side: one decoder object per stream.
pub struct Decoder {
    gpu: Arc<Gpu>,
    timeline: Timeline,
    compute: Commands,
    recombine: Recombine,
    main: H264Decoder,
    aux: Option<H264Decoder>,
    outputs: Vec<(Image, ExportedDmabuf)>,
    /// Counts output rings; a release from an older ring is ignored.
    ring: u64,
    /// Images handed out as `DisplayFrame`s and not yet dropped.
    busy: Vec<bool>,
    /// Busy images in hand-out order, oldest first.
    handed_out: std::collections::VecDeque<usize>,
    release_tx: Sender<(u64, usize)>,
    release_rx: Receiver<(u64, usize)>,
    /// CPU readback staging, allocated on first use (tests and the probe).
    readback: Option<HostBuffer>,
    width: u32,
    height: u32,
    zoom: u32,
    /// The last `decode_to_output` produced a picture pair that is complete
    /// and in shader-read layout, so `redraw` may sample it.
    last_complete: bool,
}

impl Decoder {
    pub fn new(gpu: &Arc<Gpu>, dual: bool, width: u32, height: u32) -> Result<Self> {
        let (release_tx, release_rx) = channel();
        let outputs = Self::output_ring(gpu, width, height)?;
        Ok(Self {
            gpu: gpu.clone(),
            timeline: Timeline::new(gpu)?,
            compute: Commands::new(gpu, gpu.families.compute, gpu.compute_queue)?,
            recombine: Recombine::new(gpu)?,
            main: H264Decoder::new(gpu)?,
            aux: if dual {
                Some(H264Decoder::new(gpu)?)
            } else {
                None
            },
            outputs,
            ring: 0,
            busy: vec![false; DISPLAY_RING],
            handed_out: std::collections::VecDeque::new(),
            release_tx,
            release_rx,
            readback: None,
            width,
            height,
            zoom: 1,
            last_complete: false,
        })
    }

    fn output_ring(
        gpu: &Arc<Gpu>,
        width: u32,
        height: u32,
    ) -> Result<Vec<(Image, ExportedDmabuf)>> {
        (0..DISPLAY_RING)
            .map(|_| {
                let img = Image::exportable_bgra(gpu, width, height)?;
                let dmabuf = img.export_dmabuf()?;
                Ok((img, dmabuf))
            })
            .collect()
    }

    /// Write each stream pixel as a `zoom` x `zoom` block, so the display
    /// side shows the frame at that integer scale with no resampling. The
    /// request is capped so the output fits the device's image size limit.
    /// Returns the zoom in effect. Frames already handed out keep their old
    /// images alive through their dmabuf fds.
    pub fn set_zoom(&mut self, zoom: u32) -> Result<u32> {
        // SAFETY: valid instance and physical device handles.
        let max_side = unsafe {
            self.gpu
                .instance
                .get_physical_device_properties(self.gpu.physical)
                .limits
                .max_image_dimension2_d
        };
        let cap = (max_side / self.width.max(1)).min(max_side / self.height.max(1));
        let zoom = zoom.clamp(1, cap.max(1));
        if zoom == self.zoom {
            return Ok(zoom);
        }
        self.compute.wait()?;
        let outputs = Self::output_ring(&self.gpu, self.width * zoom, self.height * zoom)?;
        self.outputs = outputs;
        self.ring += 1;
        self.busy = vec![false; DISPLAY_RING];
        self.handed_out.clear();
        self.readback = None;
        self.zoom = zoom;
        Ok(zoom)
    }

    pub fn zoom(&self) -> u32 {
        self.zoom
    }

    /// Recombine the last decoded picture again into a fresh display frame,
    /// as after a zoom change on a still screen. `None` before the first
    /// complete picture, or after a decode that failed part way.
    pub fn redraw(&mut self) -> Result<Option<DisplayFrame>> {
        if !self.last_complete {
            return Ok(None);
        }
        let idx = self.free_output();
        let (dst, _) = &self.outputs[idx];
        let (recombine, w, h, zoom) = (&self.recombine, self.width, self.height, self.zoom);
        let Some(main_img) = self.main.last_output() else {
            return Ok(None);
        };
        let aux_img = match &self.aux {
            Some(a) => match a.last_output() {
                Some(img) => Some(img),
                None => return Ok(None),
            },
            None => None,
        };
        self.compute
            .run(self.timeline.semaphore, None, None, true, |cmd| {
                main_img.transition(cmd, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
                if let Some(a) = aux_img {
                    a.transition(cmd, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
                }
                dst.transition(cmd, vk::ImageLayout::GENERAL);
                recombine.record(cmd, main_img, aux_img, dst, (w, h), zoom)?;
                dst.memory_barrier(cmd);
                Ok(())
            })?;
        Ok(Some(self.hand_out(idx)?))
    }

    fn hand_out(&mut self, idx: usize) -> Result<DisplayFrame> {
        let (_, dmabuf) = &self.outputs[idx];
        self.busy[idx] = true;
        self.handed_out.push_back(idx);
        Ok(DisplayFrame {
            fd: dmabuf.fd.as_fd().try_clone_to_owned()?,
            width: dmabuf.width,
            height: dmabuf.height,
            stride: dmabuf.stride,
            offset: dmabuf.offset,
            fourcc: dmabuf.fourcc,
            modifier: dmabuf.modifier,
            zoom: self.zoom,
            ring: self.ring,
            index: idx,
            release: self.release_tx.clone(),
        })
    }

    /// Decode one access unit pair and recombine to a display frame. Blocks
    /// until the frame is complete. `None` when the unit had no picture.
    pub fn decode(&mut self, main: &[u8], aux: &[u8]) -> Result<Option<DisplayFrame>> {
        let idx = self.decode_to_output(main, aux)?;
        let Some(idx) = idx else { return Ok(None) };
        Ok(Some(self.hand_out(idx)?))
    }

    /// Decode and read the BGRA pixels back to the CPU (tests and the probe).
    pub fn decode_to_bgra(&mut self, main: &[u8], aux: &[u8]) -> Result<Option<Vec<u8>>> {
        let Some(idx) = self.decode_to_output(main, aux)? else {
            return Ok(None);
        };
        let (image, _) = &self.outputs[idx];
        let (w, h) = (
            self.width as usize * self.zoom as usize,
            self.height as usize * self.zoom as usize,
        );
        let size = w * h * 4;
        if self.readback.is_none() {
            self.readback = Some(HostBuffer::new(
                &self.gpu,
                size,
                vk::BufferUsageFlags::TRANSFER_DST,
                None,
            )?);
        }
        let buf = self.readback.as_ref().expect("allocated above");
        self.compute
            .run(self.timeline.semaphore, None, None, true, |cmd| {
                image.copy_rgba_to_buffer(cmd, buf);
                Ok(())
            })?;
        Ok(Some(buf.read(0, size)))
    }

    /// An output image no `DisplayFrame` holds. Waits briefly for a release
    /// when all are out; a display that never releases gets the oldest reused.
    fn free_output(&mut self) -> usize {
        loop {
            while let Ok(r) = self.release_rx.try_recv() {
                self.mark_free(r);
            }
            if let Some(i) = self.busy.iter().position(|b| !b) {
                return i;
            }
            match self.release_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(r) => self.mark_free(r),
                Err(_) => {
                    // Reclaim only the image handed out longest ago: it is the
                    // one least likely to still be on screen.
                    if let Some(i) = self.handed_out.pop_front() {
                        tracing::warn!(image = i, "display frame not released; reusing the oldest");
                        self.busy[i] = false;
                    }
                }
            }
        }
    }

    fn mark_free(&mut self, (ring, i): (u64, usize)) {
        if ring != self.ring {
            return;
        }
        self.busy[i] = false;
        self.handed_out.retain(|&h| h != i);
    }

    fn decode_to_output(&mut self, main: &[u8], aux: &[u8]) -> Result<Option<usize>> {
        let t0 = std::time::Instant::now();
        self.last_complete = false;
        let idx = self.free_output();
        let main_done = self.timeline.advance();
        let Some(main_img) = self.main.decode(main, &self.timeline, main_done)? else {
            return Ok(None);
        };
        let t_main = t0.elapsed();
        let (aux_img, wait) = match &mut self.aux {
            Some(dec) => {
                let aux_done = self.timeline.advance();
                match dec.decode(aux, &self.timeline, aux_done)? {
                    Some(img) => (Some(img), aux_done),
                    None => return Ok(None),
                }
            }
            None => (None, main_done),
        };
        let t_aux = t0.elapsed() - t_main;
        let (dst, _) = &self.outputs[idx];
        let (recombine, w, h, zoom) = (&self.recombine, self.width, self.height, self.zoom);
        self.compute
            .run(self.timeline.semaphore, Some(wait), None, true, |cmd| {
                main_img.transition(cmd, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
                if let Some(a) = aux_img {
                    a.transition(cmd, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
                }
                dst.transition(cmd, vk::ImageLayout::GENERAL);
                recombine.record(cmd, main_img, aux_img, dst, (w, h), zoom)?;
                dst.memory_barrier(cmd);
                Ok(())
            })?;
        self.last_complete = true;
        tracing::debug!(
            submit_main_us = t_main.as_micros(),
            submit_aux_us = t_aux.as_micros(),
            total_us = t0.elapsed().as_micros(),
            "decode + recombine"
        );
        Ok(Some(idx))
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        let _ = self.compute.wait();
        let _ = self.main.wait();
        if let Some(a) = &self.aux {
            let _ = a.wait();
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        let _ = self.compute.wait();
    }
}
