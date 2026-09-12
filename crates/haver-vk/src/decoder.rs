//! H.264 decode through `VK_KHR_video_decode_h264`.
//!
//! The application supplies what the hardware does not parse: SPS/PPS as
//! `StdVideo` structs, the per-picture header fields, picture order counts,
//! and the DPB (reference slot) state. Decoded pictures land in a small ring
//! of output images that the recombine pass reads.

use std::sync::Arc;

use ash::vk;
use ash::vk::native as std_video;

use crate::device::{Commands, Gpu, Timeline};
use crate::encoder::{std_header, with_h264_profile, Session, SessionParameters};
use crate::h264::annexb::{nal_units, NAL_IDR, NAL_PPS, NAL_SLICE, NAL_SPS};
use crate::h264::parser::{
    parse_pps, parse_slice_header, parse_sps, Pps, SliceHeader, SliceType, Sps,
};
use crate::image::{HostBuffer, Image, Role, NV12};
use crate::{zeroed, Error, Result};

/// Output images kept per decoder; the caller reads one while the next
/// decode writes another.
const OUTPUT_RING: usize = 2;

#[derive(Clone, Copy)]
struct DpbPicture {
    frame_num: u32,
    poc: i32,
}

struct Stream {
    sps: Sps,
    pps: Pps,
    session: Session,
    params: SessionParameters,
    dpb: Image,
    slots: Vec<Option<DpbPicture>>,
    outputs: Vec<Image>,
    next_output: usize,
    coded: vk::Extent2D,
    started: bool,
}

pub struct H264Decoder {
    gpu: Arc<Gpu>,
    commands: Commands,
    bitstream: HostBuffer,
    bitstream_align: usize,
    max_dpb_slots: u32,
    sps_bytes: Vec<u8>,
    pps_bytes: Vec<u8>,
    stream: Option<Stream>,
    poc: PocState,
}

/// Picture order count derivation state (8.2.1), POC types 0 and 2.
#[derive(Default)]
struct PocState {
    prev_msb: i32,
    prev_lsb: u32,
    prev_frame_num_offset: u32,
    prev_frame_num: u32,
}

impl H264Decoder {
    pub fn new(gpu: &Arc<Gpu>) -> Result<Self> {
        let family = gpu.decode_family()?;
        let queue = gpu
            .decode_queue
            .ok_or_else(|| Error::Unsupported("no decode queue".into()))?;
        let (align, max_dpb_slots) = with_h264_profile(false, |profile| {
            let mut h264_caps = vk::VideoDecodeH264CapabilitiesKHR::default();
            let mut dec_caps = vk::VideoDecodeCapabilitiesKHR::default();
            let mut caps = vk::VideoCapabilitiesKHR::default()
                .push_next(&mut dec_caps)
                .push_next(&mut h264_caps);
            // SAFETY: valid physical device and chained structs.
            unsafe {
                (gpu.video_instance
                    .fp()
                    .get_physical_device_video_capabilities_khr)(
                    gpu.physical, profile, &mut caps
                )
                .result()?
            };
            let align = caps
                .min_bitstream_buffer_size_alignment
                .max(caps.min_bitstream_buffer_offset_alignment) as usize;
            let max_dpb_slots = caps.max_dpb_slots;
            if !dec_caps
                .flags
                .contains(vk::VideoDecodeCapabilityFlagsKHR::DPB_AND_OUTPUT_DISTINCT)
            {
                return Err(Error::Unsupported(
                    "decoder requires DPB and output to coincide; not implemented".into(),
                ));
            }
            Ok((align, max_dpb_slots))
        })?;
        let bitstream = with_h264_profile(false, |profile| {
            HostBuffer::new(
                gpu,
                4 << 20,
                vk::BufferUsageFlags::VIDEO_DECODE_SRC_KHR,
                Some(profile),
            )
        })?;
        Ok(Self {
            gpu: gpu.clone(),
            commands: Commands::new(gpu, family, queue)?,
            bitstream,
            bitstream_align: align.max(1),
            max_dpb_slots,
            sps_bytes: Vec::new(),
            pps_bytes: Vec::new(),
            stream: None,
            poc: PocState::default(),
        })
    }

    /// Display size of the current stream, once an SPS has been seen.
    pub fn display_size(&self) -> Option<(u32, u32)> {
        self.stream.as_ref().map(|s| s.sps.display_size())
    }

    /// Decode one access unit. Returns the output image once its decode has
    /// been submitted; it is complete when the timeline reaches `signal`.
    /// Returns `None` for an access unit with no picture (parameter sets only).
    pub(crate) fn decode(
        &mut self,
        access_unit: &[u8],
        timeline: &Timeline,
        signal: u64,
    ) -> Result<Option<&Image>> {
        let nals = nal_units(access_unit);
        let mut slices = Vec::new();
        let mut header = None;
        for nal in &nals {
            match nal.nal_type {
                NAL_SPS => {
                    if nal.data != self.sps_bytes.as_slice() {
                        let sps = parse_sps(nal.data)?;
                        self.sps_bytes = nal.data.to_vec();
                        self.pps_bytes.clear();
                        self.open_stream(sps, None)?;
                    }
                }
                NAL_PPS => {
                    if nal.data != self.pps_bytes.as_slice() {
                        let sps = self.stream.as_ref().map(|s| s.sps.clone());
                        let pps = parse_pps(nal.data, |_| sps.clone())?;
                        self.pps_bytes = nal.data.to_vec();
                        let stream = self
                            .stream
                            .as_mut()
                            .ok_or(Error::Bitstream("PPS before SPS"))?;
                        stream.params =
                            create_params(&self.gpu, stream.session.handle, &stream.sps, &pps)?;
                        stream.pps = pps;
                    }
                }
                NAL_SLICE | NAL_IDR => {
                    if header.is_none() {
                        let stream = self
                            .stream
                            .as_ref()
                            .ok_or(Error::Bitstream("slice before parameter sets"))?;
                        header = Some(parse_slice_header(nal.data, |_| {
                            Some((stream.pps.clone(), stream.sps.clone()))
                        })?);
                    }
                    slices.push(nal.data);
                }
                _ => {}
            }
        }
        let Some(header) = header else {
            return Ok(None);
        };
        let stream = self.stream.as_mut().ok_or(Error::Bitstream("no stream"))?;

        // Bitstream buffer: each slice with a 3-byte start code, padded to
        // the alignment the decoder asks for.
        let total: usize = slices.iter().map(|s| s.len() + 3).sum();
        let range = total.div_ceil(self.bitstream_align) * self.bitstream_align;
        if range > self.bitstream.size {
            self.commands.wait()?;
            self.bitstream = with_h264_profile(false, |profile| {
                HostBuffer::new(
                    &self.gpu,
                    range.next_power_of_two(),
                    vk::BufferUsageFlags::VIDEO_DECODE_SRC_KHR,
                    Some(profile),
                )
            })?;
        }
        let mut offsets = Vec::with_capacity(slices.len());
        let mut pos = 0;
        for s in &slices {
            offsets.push(pos as u32);
            self.bitstream.write(pos, &[0, 0, 1]);
            self.bitstream.write(pos + 3, s);
            pos += s.len() + 3;
        }
        if range > total {
            self.bitstream.write(total, &vec![0u8; range - total]);
        }

        let poc = self.poc.derive(&stream.sps, &header)?;
        let is_ref = header.is_reference();
        if header.is_idr() {
            stream.slots.iter_mut().for_each(|s| *s = None);
        }
        // The reconstructed picture needs a free slot before decoding; the
        // marking process (which frees old references) runs after it.
        let setup_slot = if is_ref {
            let free = stream.slots.iter().position(Option::is_none);
            Some(match free {
                Some(i) => i,
                None => {
                    evict_oldest(&mut stream.slots, header.frame_num, &stream.sps);
                    stream
                        .slots
                        .iter()
                        .position(Option::is_none)
                        .ok_or(Error::Bitstream("no free DPB slot"))?
                }
            })
        } else {
            None
        };
        let refs: Vec<(usize, DpbPicture)> = stream
            .slots
            .iter()
            .enumerate()
            .filter_map(|(i, s)| s.map(|p| (i, p)))
            .collect();
        let output_index = stream.next_output;
        stream.next_output = (stream.next_output + 1) % OUTPUT_RING;

        let coded = stream.coded;
        let dpb_resource = |slot: usize| {
            vk::VideoPictureResourceInfoKHR::default()
                .coded_extent(coded)
                .base_array_layer(0)
                .image_view_binding(stream.dpb.layer_views[slot])
        };
        let ref_std = |p: DpbPicture| {
            let flags = zeroed::<std_video::StdVideoDecodeH264ReferenceInfoFlags>();
            std_video::StdVideoDecodeH264ReferenceInfo {
                flags,
                FrameNum: p.frame_num as u16,
                reserved: 0,
                PicOrderCnt: [p.poc, p.poc],
            }
        };

        let ref_res: Vec<vk::VideoPictureResourceInfoKHR> =
            refs.iter().map(|(i, _)| dpb_resource(*i)).collect();
        let ref_std_infos: Vec<std_video::StdVideoDecodeH264ReferenceInfo> =
            refs.iter().map(|(_, p)| ref_std(*p)).collect();
        let mut ref_dpb_a: Vec<vk::VideoDecodeH264DpbSlotInfoKHR> = ref_std_infos
            .iter()
            .map(|s| vk::VideoDecodeH264DpbSlotInfoKHR::default().std_reference_info(s))
            .collect();
        let mut ref_dpb_b: Vec<vk::VideoDecodeH264DpbSlotInfoKHR> = ref_std_infos
            .iter()
            .map(|s| vk::VideoDecodeH264DpbSlotInfoKHR::default().std_reference_info(s))
            .collect();
        let setup_res = setup_slot.map(dpb_resource);
        let mut begin_slots: Vec<vk::VideoReferenceSlotInfoKHR> = Vec::new();
        for (((i, _), res), dpb) in refs.iter().zip(&ref_res).zip(ref_dpb_a.iter_mut()) {
            begin_slots.push(
                vk::VideoReferenceSlotInfoKHR::default()
                    .slot_index(*i as i32)
                    .picture_resource(res)
                    .push_next(dpb),
            );
        }
        if let Some(res) = setup_res.as_ref() {
            begin_slots.push(
                vk::VideoReferenceSlotInfoKHR::default()
                    .slot_index(-1)
                    .picture_resource(res),
            );
        }
        let decode_refs: Vec<vk::VideoReferenceSlotInfoKHR> = refs
            .iter()
            .zip(&ref_res)
            .zip(ref_dpb_b.iter_mut())
            .map(|(((i, _), res), dpb)| {
                vk::VideoReferenceSlotInfoKHR::default()
                    .slot_index(*i as i32)
                    .picture_resource(res)
                    .push_next(dpb)
            })
            .collect();

        let current = DpbPicture {
            frame_num: header.frame_num,
            poc,
        };
        let setup_std = ref_std(current);
        let mut setup_dpb =
            vk::VideoDecodeH264DpbSlotInfoKHR::default().std_reference_info(&setup_std);
        let setup_ref = setup_slot.zip(setup_res.as_ref()).map(|(slot, res)| {
            vk::VideoReferenceSlotInfoKHR::default()
                .slot_index(slot as i32)
                .picture_resource(res)
                .push_next(&mut setup_dpb)
        });

        let mut pic_flags = zeroed::<std_video::StdVideoDecodeH264PictureInfoFlags>();
        pic_flags.set_is_intra((header.slice_type == Some(SliceType::I)) as u32);
        pic_flags.set_IdrPicFlag(header.is_idr() as u32);
        pic_flags.set_is_reference(is_ref as u32);
        let std_pic = std_video::StdVideoDecodeH264PictureInfo {
            flags: pic_flags,
            seq_parameter_set_id: stream.sps.sps_id,
            pic_parameter_set_id: header.pps_id,
            reserved1: 0,
            reserved2: 0,
            frame_num: header.frame_num as u16,
            idr_pic_id: header.idr_pic_id as u16,
            PicOrderCnt: [poc, poc],
        };
        let mut h264_pic = vk::VideoDecodeH264PictureInfoKHR::default()
            .std_picture_info(&std_pic)
            .slice_offsets(&offsets);
        let output = &stream.outputs[output_index];
        let dst = vk::VideoPictureResourceInfoKHR::default()
            .coded_extent(coded)
            .base_array_layer(0)
            .image_view_binding(output.view0());
        let mut decode_info = vk::VideoDecodeInfoKHR::default()
            .src_buffer(self.bitstream.buffer)
            .src_buffer_offset(0)
            .src_buffer_range(range as u64)
            .dst_picture_resource(dst)
            .reference_slots(&decode_refs)
            .push_next(&mut h264_pic);
        if let Some(s) = setup_ref.as_ref() {
            decode_info = decode_info.setup_reference_slot(s);
        }
        let begin = vk::VideoBeginCodingInfoKHR::default()
            .video_session(stream.session.handle)
            .video_session_parameters(stream.params.handle)
            .reference_slots(&begin_slots);

        let video = self.gpu.video.fp();
        let decode = self.gpu.decode.fp();
        let started = stream.started;
        let dpb = &stream.dpb;
        self.commands
            .run(timeline.semaphore, None, Some(signal), false, |cmd| {
                dpb.transition(cmd, vk::ImageLayout::VIDEO_DECODE_DPB_KHR);
                output.transition(cmd, vk::ImageLayout::VIDEO_DECODE_DST_KHR);
                // SAFETY: recording valid video commands on a decode-family queue;
                // every referenced struct outlives the call.
                unsafe {
                    (video.cmd_begin_video_coding_khr)(cmd, &begin);
                    if !started {
                        let control = vk::VideoCodingControlInfoKHR::default()
                            .flags(vk::VideoCodingControlFlagsKHR::RESET);
                        (video.cmd_control_video_coding_khr)(cmd, &control);
                    }
                    (decode.cmd_decode_video_khr)(cmd, &decode_info);
                    (video.cmd_end_video_coding_khr)(cmd, &vk::VideoEndCodingInfoKHR::default());
                }
                Ok(())
            })?;

        stream.started = true;
        if let Some(slot) = setup_slot {
            stream.slots[slot] = Some(current);
            match &header.mmcos {
                Some(ops) => apply_mmco(
                    &mut stream.slots,
                    ops,
                    header.frame_num,
                    stream.sps.max_frame_num(),
                ),
                None => sliding_window(&mut stream.slots, header.frame_num, &stream.sps),
            }
        }
        if is_ref {
            self.poc.prev_frame_num = header.frame_num;
        }
        Ok(Some(&stream.outputs[output_index]))
    }

    /// Wait for all submitted decodes to finish (before teardown or resize).
    pub fn wait(&self) -> Result<()> {
        self.commands.wait()
    }

    fn open_stream(&mut self, sps: Sps, pps: Option<Pps>) -> Result<()> {
        let (cw, ch) = sps.coded_size();
        let coded = vk::Extent2D {
            width: cw,
            height: ch,
        };
        let refs = (sps.max_num_ref_frames as u32).max(1);
        let dpb_slots = (refs + 1).min(self.max_dpb_slots);
        self.commands.wait()?;
        self.stream = None;
        let stream = with_h264_profile(false, |profile| {
            let header = std_header(false);
            let info = vk::VideoSessionCreateInfoKHR::default()
                .queue_family_index(self.gpu.decode_family()?)
                .video_profile(profile)
                .picture_format(NV12)
                .max_coded_extent(coded)
                .reference_picture_format(NV12)
                .max_dpb_slots(dpb_slots)
                .max_active_reference_pictures(refs.min(dpb_slots - 1).max(1))
                .std_header_version(&header);
            let session = Session::new(&self.gpu, &info)?;
            let pps = pps.unwrap_or_default();
            let params = create_params(&self.gpu, session.handle, &sps, &pps)?;
            let dpb = Image::nv12(&self.gpu, Role::DecodeDpb, cw, ch, dpb_slots, Some(profile))?;
            let outputs = (0..OUTPUT_RING)
                .map(|_| Image::nv12(&self.gpu, Role::DecodeOutput, cw, ch, 1, Some(profile)))
                .collect::<Result<Vec<_>>>()?;
            Ok::<_, Error>(Stream {
                sps,
                pps,
                session,
                params,
                dpb,
                slots: vec![None; dpb_slots as usize],
                outputs,
                next_output: 0,
                coded,
                started: false,
            })
        })?;
        tracing::info!(coded = ?coded, dpb_slots, "vulkan decoder stream opened");
        self.stream = Some(stream);
        self.poc = PocState::default();
        Ok(())
    }
}

impl PocState {
    /// Picture order count per 8.2.1 for POC types 0 and 2 (frames only).
    fn derive(&mut self, sps: &Sps, h: &SliceHeader) -> Result<i32> {
        match sps.pic_order_cnt_type {
            0 => {
                let max = sps.max_poc_lsb() as i32;
                let (prev_msb, prev_lsb) = if h.is_idr() {
                    (0, 0)
                } else {
                    (self.prev_msb, self.prev_lsb as i32)
                };
                let lsb = h.pic_order_cnt_lsb as i32;
                let msb = if lsb < prev_lsb && prev_lsb - lsb >= max / 2 {
                    prev_msb + max
                } else if lsb > prev_lsb && lsb - prev_lsb > max / 2 {
                    prev_msb - max
                } else {
                    prev_msb
                };
                if h.is_reference() {
                    self.prev_msb = msb;
                    self.prev_lsb = h.pic_order_cnt_lsb;
                }
                Ok(msb + lsb)
            }
            2 => {
                let offset = if h.is_idr() {
                    0
                } else if self.prev_frame_num > h.frame_num {
                    self.prev_frame_num_offset + sps.max_frame_num()
                } else {
                    self.prev_frame_num_offset
                };
                self.prev_frame_num_offset = offset;
                let n = (offset + h.frame_num) as i32;
                Ok(if h.is_idr() {
                    0
                } else if h.is_reference() {
                    2 * n
                } else {
                    2 * n - 1
                })
            }
            _ => Err(Error::Bitstream("pic_order_cnt_type 1 is not supported")),
        }
    }
}

/// Sliding-window marking (8.2.5.3): after the current picture is stored,
/// drop the oldest references until `max_num_ref_frames` remain.
fn sliding_window(slots: &mut [Option<DpbPicture>], current_frame_num: u32, sps: &Sps) {
    let max_refs = (sps.max_num_ref_frames as usize).max(1);
    while slots.iter().flatten().count() > max_refs {
        evict_oldest(slots, current_frame_num, sps);
    }
}

/// Free the slot with the smallest FrameNumWrap.
fn evict_oldest(slots: &mut [Option<DpbPicture>], current_frame_num: u32, sps: &Sps) {
    let wrap = |f: u32| {
        if f > current_frame_num {
            f as i64 - sps.max_frame_num() as i64
        } else {
            f as i64
        }
    };
    let oldest = slots
        .iter()
        .enumerate()
        .filter_map(|(i, s)| s.map(|p| (wrap(p.frame_num), i)))
        .min();
    if let Some((_, i)) = oldest {
        slots[i] = None;
    }
}

/// Adaptive reference marking: only the short-term operations our streams
/// can contain (1 = unmark one picture, 5 = unmark all). Long-term marking is
/// not produced by the encoder and is ignored with a warning.
fn apply_mmco(
    slots: &mut [Option<DpbPicture>],
    ops: &[crate::h264::parser::Mmco],
    current_frame_num: u32,
    max_frame_num: u32,
) {
    for op in ops {
        match op.op {
            1 => {
                let pic_num =
                    current_frame_num as i64 - (op.difference_of_pic_nums_minus1 as i64 + 1);
                for s in slots.iter_mut() {
                    let matches = s.is_some_and(|p| {
                        let wrap = if p.frame_num > current_frame_num {
                            p.frame_num as i64 - max_frame_num as i64
                        } else {
                            p.frame_num as i64
                        };
                        wrap == pic_num
                    });
                    if matches {
                        *s = None;
                    }
                }
            }
            5 => slots.iter_mut().for_each(|s| *s = None),
            other => tracing::warn!(
                op = other,
                "ignoring unsupported memory_management_control_operation"
            ),
        }
    }
}

fn create_params(
    gpu: &Arc<Gpu>,
    session: vk::VideoSessionKHR,
    sps: &Sps,
    pps: &Pps,
) -> Result<SessionParameters> {
    let sps_lists = sps.scaling_lists.as_ref().map(std_scaling_lists);
    let pps_lists = pps.scaling_lists.as_ref().map(std_scaling_lists);
    let std_sps = std_sps(sps, sps_lists.as_ref());
    let std_pps = std_pps(pps, pps_lists.as_ref());
    let spss = [std_sps];
    let ppss = [std_pps];
    let add = vk::VideoDecodeH264SessionParametersAddInfoKHR::default()
        .std_sp_ss(&spss)
        .std_pp_ss(&ppss);
    let mut h264 = vk::VideoDecodeH264SessionParametersCreateInfoKHR::default()
        .max_std_sps_count(1)
        .max_std_pps_count(1)
        .parameters_add_info(&add);
    let info = vk::VideoSessionParametersCreateInfoKHR::default()
        .video_session(session)
        .push_next(&mut h264);
    SessionParameters::new(gpu, &info)
}

fn std_scaling_lists(l: &crate::h264::parser::ScalingLists) -> std_video::StdVideoH264ScalingLists {
    std_video::StdVideoH264ScalingLists {
        scaling_list_present_mask: l.present_mask,
        use_default_scaling_matrix_mask: l.use_default_mask,
        ScalingList4x4: l.list_4x4,
        ScalingList8x8: l.list_8x8,
    }
}

fn std_sps(
    s: &Sps,
    lists: Option<&std_video::StdVideoH264ScalingLists>,
) -> std_video::StdVideoH264SequenceParameterSet {
    let mut flags = zeroed::<std_video::StdVideoH264SpsFlags>();
    flags.set_constraint_set0_flag((s.constraint_flags >> 7) as u32 & 1);
    flags.set_constraint_set1_flag((s.constraint_flags >> 6) as u32 & 1);
    flags.set_constraint_set2_flag((s.constraint_flags >> 5) as u32 & 1);
    flags.set_constraint_set3_flag((s.constraint_flags >> 4) as u32 & 1);
    flags.set_direct_8x8_inference_flag(s.direct_8x8_inference as u32);
    flags.set_mb_adaptive_frame_field_flag(s.mb_adaptive_frame_field as u32);
    flags.set_frame_mbs_only_flag(s.frame_mbs_only as u32);
    flags.set_delta_pic_order_always_zero_flag(s.delta_pic_order_always_zero as u32);
    flags.set_separate_colour_plane_flag(s.separate_colour_plane as u32);
    flags.set_gaps_in_frame_num_value_allowed_flag(s.gaps_in_frame_num_allowed as u32);
    flags.set_qpprime_y_zero_transform_bypass_flag(s.qpprime_y_zero_transform_bypass as u32);
    flags.set_frame_cropping_flag(s.frame_cropping.is_some() as u32);
    flags.set_seq_scaling_matrix_present_flag(lists.is_some() as u32);
    flags.set_vui_parameters_present_flag(0);
    let crop = s.frame_cropping.unwrap_or([0; 4]);
    std_video::StdVideoH264SequenceParameterSet {
        flags,
        profile_idc: s.profile_idc as std_video::StdVideoH264ProfileIdc,
        level_idc: level_idc(s.level_idc, s.constraint_flags & 0x10 != 0),
        chroma_format_idc: s.chroma_format_idc as std_video::StdVideoH264ChromaFormatIdc,
        seq_parameter_set_id: s.sps_id,
        bit_depth_luma_minus8: s.bit_depth_luma_minus8,
        bit_depth_chroma_minus8: s.bit_depth_chroma_minus8,
        log2_max_frame_num_minus4: s.log2_max_frame_num_minus4,
        pic_order_cnt_type: s.pic_order_cnt_type as std_video::StdVideoH264PocType,
        offset_for_non_ref_pic: s.offset_for_non_ref_pic,
        offset_for_top_to_bottom_field: s.offset_for_top_to_bottom_field,
        log2_max_pic_order_cnt_lsb_minus4: s.log2_max_pic_order_cnt_lsb_minus4,
        num_ref_frames_in_pic_order_cnt_cycle: s.offset_for_ref_frame.len() as u8,
        max_num_ref_frames: s.max_num_ref_frames,
        reserved1: 0,
        pic_width_in_mbs_minus1: s.pic_width_in_mbs_minus1,
        pic_height_in_map_units_minus1: s.pic_height_in_map_units_minus1,
        frame_crop_left_offset: crop[0],
        frame_crop_right_offset: crop[1],
        frame_crop_top_offset: crop[2],
        frame_crop_bottom_offset: crop[3],
        reserved2: 0,
        pOffsetForRefFrame: if s.offset_for_ref_frame.is_empty() {
            std::ptr::null()
        } else {
            s.offset_for_ref_frame.as_ptr()
        },
        pScalingLists: lists.map_or(std::ptr::null(), |l| l as *const _),
        pSequenceParameterSetVui: std::ptr::null(),
    }
}

fn std_pps(
    p: &Pps,
    lists: Option<&std_video::StdVideoH264ScalingLists>,
) -> std_video::StdVideoH264PictureParameterSet {
    let mut flags = zeroed::<std_video::StdVideoH264PpsFlags>();
    flags.set_transform_8x8_mode_flag(p.transform_8x8_mode as u32);
    flags.set_redundant_pic_cnt_present_flag(p.redundant_pic_cnt_present as u32);
    flags.set_constrained_intra_pred_flag(p.constrained_intra_pred as u32);
    flags.set_deblocking_filter_control_present_flag(p.deblocking_filter_control_present as u32);
    flags.set_weighted_pred_flag(p.weighted_pred as u32);
    flags.set_bottom_field_pic_order_in_frame_present_flag(
        p.bottom_field_pic_order_in_frame_present as u32,
    );
    flags.set_entropy_coding_mode_flag(p.entropy_coding_mode as u32);
    flags.set_pic_scaling_matrix_present_flag(lists.is_some() as u32);
    std_video::StdVideoH264PictureParameterSet {
        flags,
        seq_parameter_set_id: p.sps_id,
        pic_parameter_set_id: p.pps_id,
        num_ref_idx_l0_default_active_minus1: p.num_ref_idx_l0_default_active_minus1,
        num_ref_idx_l1_default_active_minus1: p.num_ref_idx_l1_default_active_minus1,
        weighted_bipred_idc: p.weighted_bipred_idc as std_video::StdVideoH264WeightedBipredIdc,
        pic_init_qp_minus26: p.pic_init_qp_minus26,
        pic_init_qs_minus26: p.pic_init_qs_minus26,
        chroma_qp_index_offset: p.chroma_qp_index_offset,
        second_chroma_qp_index_offset: p.second_chroma_qp_index_offset,
        pScalingLists: lists.map_or(std::ptr::null(), |l| l as *const _),
    }
}

/// Map a `level_idc` byte to the Vulkan enum (1.0 .. 6.2; 1b via the
/// constraint_set3 flag at level 11).
fn level_idc(level: u8, set3: bool) -> std_video::StdVideoH264LevelIdc {
    use std_video::*;
    match level {
        10 => StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_1_0,
        11 if set3 => StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_1_0,
        11 => StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_1_1,
        12 => StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_1_2,
        13 => StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_1_3,
        20 => StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_2_0,
        21 => StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_2_1,
        22 => StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_2_2,
        30 => StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_3_0,
        31 => StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_3_1,
        32 => StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_3_2,
        40 => StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_4_0,
        41 => StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_4_1,
        42 => StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_4_2,
        50 => StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_5_0,
        51 => StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_5_1,
        52 => StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_5_2,
        60 => StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_6_0,
        61 => StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_6_1,
        _ => StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_6_2,
    }
}

impl Drop for H264Decoder {
    fn drop(&mut self) {
        let _ = self.commands.wait();
    }
}
