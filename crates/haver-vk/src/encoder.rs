//! H.264 encode through `VK_KHR_video_encode_h264`.
//!
//! Low-delay layout: IDR then P frames, one reference, no reordering. The DPB
//! is one array image with two layers; each frame is reconstructed into one
//! layer while it predicts from the other.

use std::sync::Arc;

use ash::vk;
use ash::vk::native as std_video;

use crate::device::{Commands, Gpu, Timeline};
use crate::h264::annexb::has_start_code;
use crate::image::{HostBuffer, Image, Role, NV12};
use crate::{zeroed, Error, Result};

#[derive(Debug, Clone)]
pub struct EncoderSettings {
    pub width: u32,
    pub height: u32,
    /// Target bitrate in bits per second (CBR).
    pub bitrate: u32,
    pub framerate: u32,
}

impl EncoderSettings {
    /// A rough default bitrate for a desktop stream at this size and rate.
    pub fn default_bitrate(width: u32, height: u32, framerate: u32) -> u32 {
        // ~0.1 bits per pixel per frame keeps text crisp on a LAN.
        ((width as u64 * height as u64 * framerate as u64) / 10).min(80_000_000) as u32
    }

    pub fn coded_width(&self) -> u32 {
        self.width.div_ceil(16) * 16
    }

    pub fn coded_height(&self) -> u32 {
        self.height.div_ceil(16) * 16
    }
}

#[derive(Debug, Clone)]
pub struct EncodedPacket {
    pub keyframe: bool,
    /// Annex B access unit; an IDR carries the SPS and PPS in front.
    pub data: Vec<u8>,
}

const STD_ENCODE_NAME: &std::ffi::CStr = c"VK_STD_vulkan_video_codec_h264_encode";
const STD_DECODE_NAME: &std::ffi::CStr = c"VK_STD_vulkan_video_codec_h264_decode";
pub(crate) const STD_VERSION: u32 = vk::make_api_version(0, 1, 0, 0);
const NO_REFERENCE: u8 = 0xff;
const LOG2_MAX_FRAME_NUM_MINUS4: u8 = 12;
const LOG2_MAX_POC_LSB_MINUS4: u8 = 12;

/// The video profile every video object of one codec direction is created
/// against. Rebuilt on demand because the `ash` structs borrow their chain.
pub(crate) fn with_h264_profile<R>(
    encode: bool,
    f: impl FnOnce(&vk::VideoProfileInfoKHR) -> R,
) -> R {
    let mut enc = vk::VideoEncodeH264ProfileInfoKHR::default()
        .std_profile_idc(std_video::StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_HIGH);
    let mut usage = vk::VideoEncodeUsageInfoKHR::default()
        .video_usage_hints(vk::VideoEncodeUsageFlagsKHR::STREAMING)
        .video_content_hints(vk::VideoEncodeContentFlagsKHR::DESKTOP)
        .tuning_mode(vk::VideoEncodeTuningModeKHR::LOW_LATENCY);
    let mut dec = vk::VideoDecodeH264ProfileInfoKHR::default()
        .std_profile_idc(std_video::StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_HIGH)
        .picture_layout(vk::VideoDecodeH264PictureLayoutFlagsKHR::PROGRESSIVE);
    let base = vk::VideoProfileInfoKHR::default()
        .chroma_subsampling(vk::VideoChromaSubsamplingFlagsKHR::TYPE_420)
        .luma_bit_depth(vk::VideoComponentBitDepthFlagsKHR::TYPE_8)
        .chroma_bit_depth(vk::VideoComponentBitDepthFlagsKHR::TYPE_8);
    let profile = if encode {
        base.video_codec_operation(vk::VideoCodecOperationFlagsKHR::ENCODE_H264)
            .push_next(&mut enc)
            .push_next(&mut usage)
    } else {
        base.video_codec_operation(vk::VideoCodecOperationFlagsKHR::DECODE_H264)
            .push_next(&mut dec)
    };
    f(&profile)
}

pub(crate) fn std_header(encode: bool) -> vk::ExtensionProperties {
    let mut props = vk::ExtensionProperties::default().spec_version(STD_VERSION);
    let name = if encode {
        STD_ENCODE_NAME
    } else {
        STD_DECODE_NAME
    };
    for (dst, src) in props
        .extension_name
        .iter_mut()
        .zip(name.to_bytes_with_nul())
    {
        *dst = *src as std::ffi::c_char;
    }
    props
}

/// A video session with its bound memory.
pub(crate) struct Session {
    gpu: Arc<Gpu>,
    pub(crate) handle: vk::VideoSessionKHR,
    memory: Vec<vk::DeviceMemory>,
}

impl Session {
    pub(crate) fn new(gpu: &Arc<Gpu>, info: &vk::VideoSessionCreateInfoKHR) -> Result<Self> {
        // SAFETY: valid create info; every returned requirement is bound.
        unsafe {
            let mut handle = vk::VideoSessionKHR::null();
            (gpu.video.fp().create_video_session_khr)(
                gpu.device.handle(),
                info,
                std::ptr::null(),
                &mut handle,
            )
            .result()?;
            let mut count = 0u32;
            (gpu.video.fp().get_video_session_memory_requirements_khr)(
                gpu.device.handle(),
                handle,
                &mut count,
                std::ptr::null_mut(),
            )
            .result()?;
            let mut reqs = vec![vk::VideoSessionMemoryRequirementsKHR::default(); count as usize];
            (gpu.video.fp().get_video_session_memory_requirements_khr)(
                gpu.device.handle(),
                handle,
                &mut count,
                reqs.as_mut_ptr(),
            )
            .result()?;
            let mut memory = Vec::new();
            let mut binds = Vec::new();
            for r in &reqs {
                let mem = gpu.allocate(
                    r.memory_requirements,
                    vk::MemoryPropertyFlags::empty(),
                    None,
                )?;
                memory.push(mem);
                binds.push(
                    vk::BindVideoSessionMemoryInfoKHR::default()
                        .memory_bind_index(r.memory_bind_index)
                        .memory(mem)
                        .memory_offset(0)
                        .memory_size(r.memory_requirements.size),
                );
            }
            (gpu.video.fp().bind_video_session_memory_khr)(
                gpu.device.handle(),
                handle,
                binds.len() as u32,
                binds.as_ptr(),
            )
            .result()?;
            Ok(Self {
                gpu: gpu.clone(),
                handle,
                memory,
            })
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: owners wait for GPU work before dropping.
        unsafe {
            (self.gpu.video.fp().destroy_video_session_khr)(
                self.gpu.device.handle(),
                self.handle,
                std::ptr::null(),
            );
            for m in self.memory.drain(..) {
                self.gpu.device.free_memory(m, None);
            }
        }
    }
}

pub(crate) struct SessionParameters {
    gpu: Arc<Gpu>,
    pub(crate) handle: vk::VideoSessionParametersKHR,
}

impl SessionParameters {
    pub(crate) fn new(
        gpu: &Arc<Gpu>,
        info: &vk::VideoSessionParametersCreateInfoKHR,
    ) -> Result<Self> {
        let mut handle = vk::VideoSessionParametersKHR::null();
        // SAFETY: valid create info.
        unsafe {
            (gpu.video.fp().create_video_session_parameters_khr)(
                gpu.device.handle(),
                info,
                std::ptr::null(),
                &mut handle,
            )
            .result()?
        };
        Ok(Self {
            gpu: gpu.clone(),
            handle,
        })
    }
}

impl Drop for SessionParameters {
    fn drop(&mut self) {
        // SAFETY: owners wait for GPU work before dropping.
        unsafe {
            (self.gpu.video.fp().destroy_video_session_parameters_khr)(
                self.gpu.device.handle(),
                self.handle,
                std::ptr::null(),
            )
        };
    }
}

/// What the encoder knows about a reconstructed picture in a DPB slot.
#[derive(Clone, Copy)]
struct SlotPicture {
    frame_num: u32,
    poc: i32,
    idr: bool,
}

pub struct H264Encoder {
    gpu: Arc<Gpu>,
    settings: EncoderSettings,
    session: Session,
    params: SessionParameters,
    dpb: Image,
    bitstream: HostBuffer,
    query_pool: vk::QueryPool,
    commands: Commands,
    sps: Vec<u8>,
    pps: Vec<u8>,
    /// Pictures currently held in the two DPB slots.
    slots: [Option<SlotPicture>; 2],
    /// Slot of the reference for the next P frame.
    current_ref: Option<usize>,
    frame_num: u32,
    idr_pic_id: u16,
    poc: i32,
    started: bool,
}

impl H264Encoder {
    pub fn new(gpu: &Arc<Gpu>, settings: EncoderSettings) -> Result<Self> {
        let family = gpu.encode_family()?;
        let queue = gpu
            .encode_queue
            .ok_or_else(|| Error::Unsupported("no encode queue".into()))?;
        let coded = vk::Extent2D {
            width: settings.coded_width(),
            height: settings.coded_height(),
        };
        with_h264_profile(true, |profile| {
            // Capabilities: make sure the size fits and CBR exists.
            let mut h264_caps = vk::VideoEncodeH264CapabilitiesKHR::default();
            let mut enc_caps = vk::VideoEncodeCapabilitiesKHR::default();
            let mut caps = vk::VideoCapabilitiesKHR::default()
                .push_next(&mut enc_caps)
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
            if coded.width > caps.max_coded_extent.width
                || coded.height > caps.max_coded_extent.height
            {
                return Err(Error::Unsupported(format!(
                    "{}x{} exceeds the encoder maximum {:?}",
                    coded.width, coded.height, caps.max_coded_extent
                )));
            }
            if !enc_caps
                .rate_control_modes
                .contains(vk::VideoEncodeRateControlModeFlagsKHR::CBR)
            {
                return Err(Error::Unsupported("encoder has no CBR rate control".into()));
            }
            let header = std_header(true);
            let info = vk::VideoSessionCreateInfoKHR::default()
                .queue_family_index(family)
                .video_profile(profile)
                .picture_format(NV12)
                .max_coded_extent(coded)
                .reference_picture_format(NV12)
                .max_dpb_slots(2)
                .max_active_reference_pictures(1)
                .std_header_version(&header);
            let session = Session::new(gpu, &info)?;

            let (sps, pps) = std_parameter_sets(&settings, h264_caps.max_level_idc);
            let spss = [sps];
            let ppss = [pps];
            let add = vk::VideoEncodeH264SessionParametersAddInfoKHR::default()
                .std_sp_ss(&spss)
                .std_pp_ss(&ppss);
            let mut h264_params = vk::VideoEncodeH264SessionParametersCreateInfoKHR::default()
                .max_std_sps_count(1)
                .max_std_pps_count(1)
                .parameters_add_info(&add);
            let params_info = vk::VideoSessionParametersCreateInfoKHR::default()
                .video_session(session.handle)
                .push_next(&mut h264_params);
            let params = SessionParameters::new(gpu, &params_info)?;

            let dpb = Image::nv12(
                gpu,
                Role::EncodeDpb,
                coded.width,
                coded.height,
                2,
                Some(profile),
            )?;
            let size = (coded.width * coded.height * 2) as usize;
            let bitstream = HostBuffer::new(
                gpu,
                size.max(1 << 20),
                vk::BufferUsageFlags::VIDEO_ENCODE_DST_KHR,
                Some(profile),
            )?;

            let mut feedback = vk::QueryPoolVideoEncodeFeedbackCreateInfoKHR::default()
                .encode_feedback_flags(
                    vk::VideoEncodeFeedbackFlagsKHR::BITSTREAM_BUFFER_OFFSET
                        | vk::VideoEncodeFeedbackFlagsKHR::BITSTREAM_BYTES_WRITTEN,
                );
            let mut profile_copy = *profile;
            let query_info = vk::QueryPoolCreateInfo::default()
                .query_type(vk::QueryType::VIDEO_ENCODE_FEEDBACK_KHR)
                .query_count(1)
                .push_next(&mut feedback)
                .push_next(&mut profile_copy);
            // SAFETY: valid create info; destroyed in Drop.
            let query_pool = unsafe { gpu.device.create_query_pool(&query_info, None)? };

            let mut enc = Self {
                gpu: gpu.clone(),
                settings,
                session,
                params,
                dpb,
                bitstream,
                query_pool,
                commands: Commands::new(gpu, family, queue)?,
                sps: Vec::new(),
                pps: Vec::new(),
                slots: [None, None],
                current_ref: None,
                frame_num: 0,
                idr_pic_id: 0,
                poc: 0,
                started: false,
            };
            enc.sps = enc.encoded_parameters(true, false)?;
            enc.pps = enc.encoded_parameters(false, true)?;
            tracing::debug!(
                sps = enc.sps.len(),
                pps = enc.pps.len(),
                "encoder parameter sets"
            );
            Ok(enc)
        })
    }

    pub fn settings(&self) -> &EncoderSettings {
        &self.settings
    }

    /// SPS then PPS, Annex B framed.
    pub fn parameter_sets(&self) -> Vec<u8> {
        [self.sps.as_slice(), self.pps.as_slice()].concat()
    }

    /// Create an input image the split shader can write and this encoder
    /// can read.
    pub fn new_input(&self) -> Result<Image> {
        with_h264_profile(true, |profile| {
            Image::nv12(
                &self.gpu,
                Role::EncodeSource,
                self.settings.coded_width(),
                self.settings.coded_height(),
                1,
                Some(profile),
            )
        })
    }

    fn encoded_parameters(&self, sps: bool, pps: bool) -> Result<Vec<u8>> {
        let mut h264 = vk::VideoEncodeH264SessionParametersGetInfoKHR::default()
            .write_std_sps(sps)
            .write_std_pps(pps)
            .std_sps_id(0)
            .std_pps_id(0);
        let info = vk::VideoEncodeSessionParametersGetInfoKHR::default()
            .video_session_parameters(self.params.handle)
            .push_next(&mut h264);
        let fp = self
            .gpu
            .encode
            .fp()
            .get_encoded_video_session_parameters_khr;
        // SAFETY: two-call size query then fill, on a valid parameters object.
        let mut data = unsafe {
            let mut size = 0usize;
            fp(
                self.gpu.device.handle(),
                &info,
                std::ptr::null_mut(),
                &mut size,
                std::ptr::null_mut(),
            )
            .result()?;
            let mut data = vec![0u8; size];
            fp(
                self.gpu.device.handle(),
                &info,
                std::ptr::null_mut(),
                &mut size,
                data.as_mut_ptr().cast(),
            )
            .result()?;
            data.truncate(size);
            data
        };
        if !has_start_code(&data) {
            data.splice(0..0, [0, 0, 0, 1]);
        }
        Ok(data)
    }

    /// Encode `input` (already in VIDEO_ENCODE_SRC layout, or transitioned
    /// here) after the timeline reaches `wait`. Blocks until the bitstream
    /// is ready.
    pub(crate) fn submit(
        &mut self,
        input: &Image,
        timeline: &Timeline,
        wait: Option<u64>,
        force_keyframe: bool,
    ) -> Result<PendingEncode> {
        let idr = force_keyframe || !self.started || self.current_ref.is_none();
        if idr {
            self.frame_num = 0;
            self.poc = 0;
            if self.started {
                self.idr_pic_id = self.idr_pic_id.wrapping_add(1);
            }
            self.current_ref = None;
        }
        let setup_slot = match self.current_ref {
            Some(r) => 1 - r,
            None => 0,
        };
        let ref_slot = if idr { None } else { self.current_ref };
        let (w, h) = (self.settings.width, self.settings.height);
        let current = SlotPicture {
            frame_num: self.frame_num,
            poc: self.poc,
            idr,
        };

        with_h264_profile(true, |_profile| {
            let dpb_resource = |slot: usize| {
                vk::VideoPictureResourceInfoKHR::default()
                    .coded_extent(vk::Extent2D {
                        width: w,
                        height: h,
                    })
                    .base_array_layer(0)
                    .image_view_binding(self.dpb.layer_views[slot])
            };
            let ref_std = |p: SlotPicture| {
                let mut flags = zeroed::<std_video::StdVideoEncodeH264ReferenceInfoFlags>();
                flags.set_used_for_long_term_reference(0);
                std_video::StdVideoEncodeH264ReferenceInfo {
                    flags,
                    primary_pic_type: if p.idr {
                        std_video::StdVideoH264PictureType_STD_VIDEO_H264_PICTURE_TYPE_IDR
                    } else {
                        std_video::StdVideoH264PictureType_STD_VIDEO_H264_PICTURE_TYPE_P
                    },
                    FrameNum: p.frame_num,
                    PicOrderCnt: p.poc,
                    long_term_pic_num: 0,
                    long_term_frame_idx: 0,
                    temporal_id: 0,
                }
            };

            // Reference slots for the coding scope: the active reference (if
            // any) and the setup slot's resource, listed as inactive (-1).
            let setup_res = dpb_resource(setup_slot);
            let ref_res = ref_slot.map(dpb_resource);
            let mut begin_slots = vec![vk::VideoReferenceSlotInfoKHR::default()
                .slot_index(-1)
                .picture_resource(&setup_res)];
            let ref_std_info =
                ref_slot.map(|r| ref_std(self.slots[r].expect("active reference has a picture")));
            let mut ref_dpb_info = ref_std_info
                .as_ref()
                .map(|s| vk::VideoEncodeH264DpbSlotInfoKHR::default().std_reference_info(s));
            if let (Some(r), Some(res), Some(dpb_info)) =
                (ref_slot, ref_res.as_ref(), ref_dpb_info.as_mut())
            {
                begin_slots.push(
                    vk::VideoReferenceSlotInfoKHR::default()
                        .slot_index(r as i32)
                        .picture_resource(res)
                        .push_next(dpb_info),
                );
            }

            let setup_std = ref_std(current);
            let mut setup_dpb_info =
                vk::VideoEncodeH264DpbSlotInfoKHR::default().std_reference_info(&setup_std);
            let setup_ref = vk::VideoReferenceSlotInfoKHR::default()
                .slot_index(setup_slot as i32)
                .picture_resource(&setup_res)
                .push_next(&mut setup_dpb_info);
            let mut ref_dpb_info2 = ref_std_info
                .as_ref()
                .map(|s| vk::VideoEncodeH264DpbSlotInfoKHR::default().std_reference_info(s));
            let encode_refs: Vec<vk::VideoReferenceSlotInfoKHR> =
                match (ref_slot, ref_res.as_ref(), ref_dpb_info2.as_mut()) {
                    (Some(r), Some(res), Some(info)) => {
                        vec![vk::VideoReferenceSlotInfoKHR::default()
                            .slot_index(r as i32)
                            .picture_resource(res)
                            .push_next(info)]
                    }
                    _ => Vec::new(),
                };

            let mut list_flags = zeroed::<std_video::StdVideoEncodeH264ReferenceListsInfoFlags>();
            list_flags.set_ref_pic_list_modification_flag_l0(0);
            list_flags.set_ref_pic_list_modification_flag_l1(0);
            let mut ref_lists = std_video::StdVideoEncodeH264ReferenceListsInfo {
                flags: list_flags,
                num_ref_idx_l0_active_minus1: 0,
                num_ref_idx_l1_active_minus1: 0,
                RefPicList0: [NO_REFERENCE; 32],
                RefPicList1: [NO_REFERENCE; 32],
                refList0ModOpCount: 0,
                refList1ModOpCount: 0,
                refPicMarkingOpCount: 0,
                reserved1: [0; 7],
                pRefList0ModOperations: std::ptr::null(),
                pRefList1ModOperations: std::ptr::null(),
                pRefPicMarkingOperations: std::ptr::null(),
            };
            if let Some(r) = ref_slot {
                ref_lists.RefPicList0[0] = r as u8;
            }
            let mut pic_flags = zeroed::<std_video::StdVideoEncodeH264PictureInfoFlags>();
            pic_flags.set_IdrPicFlag(idr as u32);
            pic_flags.set_is_reference(1);
            let pic_type = if idr {
                std_video::StdVideoH264PictureType_STD_VIDEO_H264_PICTURE_TYPE_IDR
            } else {
                std_video::StdVideoH264PictureType_STD_VIDEO_H264_PICTURE_TYPE_P
            };
            let std_pic = std_video::StdVideoEncodeH264PictureInfo {
                flags: pic_flags,
                seq_parameter_set_id: 0,
                pic_parameter_set_id: 0,
                idr_pic_id: self.idr_pic_id,
                primary_pic_type: pic_type,
                frame_num: self.frame_num,
                PicOrderCnt: self.poc,
                temporal_id: 0,
                reserved1: [0; 3],
                pRefLists: &ref_lists,
            };
            let mut slice_flags = zeroed::<std_video::StdVideoEncodeH264SliceHeaderFlags>();
            slice_flags.set_direct_spatial_mv_pred_flag(0);
            slice_flags.set_num_ref_idx_active_override_flag(0);
            let std_slice = std_video::StdVideoEncodeH264SliceHeader {
                flags: slice_flags,
                first_mb_in_slice: 0,
                slice_type: if idr { std_video::StdVideoH264SliceType_STD_VIDEO_H264_SLICE_TYPE_I } else { std_video::StdVideoH264SliceType_STD_VIDEO_H264_SLICE_TYPE_P },
                slice_alpha_c0_offset_div2: 0,
                slice_beta_offset_div2: 0,
                slice_qp_delta: 0,
                reserved1: 0,
                cabac_init_idc: std_video::StdVideoH264CabacInitIdc_STD_VIDEO_H264_CABAC_INIT_IDC_0,
                disable_deblocking_filter_idc: std_video::StdVideoH264DisableDeblockingFilterIdc_STD_VIDEO_H264_DISABLE_DEBLOCKING_FILTER_IDC_DISABLED,
                pWeightTable: std::ptr::null(),
            };
            let slices = [vk::VideoEncodeH264NaluSliceInfoKHR::default()
                .constant_qp(0)
                .std_slice_header(&std_slice)];
            let mut h264_pic = vk::VideoEncodeH264PictureInfoKHR::default()
                .nalu_slice_entries(&slices)
                .std_picture_info(&std_pic)
                .generate_prefix_nalu(false);

            let src = vk::VideoPictureResourceInfoKHR::default()
                .coded_extent(vk::Extent2D {
                    width: w,
                    height: h,
                })
                .base_array_layer(0)
                .image_view_binding(input.view0());
            let encode_info = vk::VideoEncodeInfoKHR::default()
                .dst_buffer(self.bitstream.buffer)
                .dst_buffer_offset(0)
                .dst_buffer_range(self.bitstream.size as u64)
                .src_picture_resource(src)
                .setup_reference_slot(&setup_ref)
                .reference_slots(&encode_refs)
                .push_next(&mut h264_pic);

            // The rate control state must accompany every begin once it is
            // set; the first begin sets it with a control command instead.
            let mut rc = RateControl::new(&self.settings);
            let (mut rc_info, mut h264_rc) = rc.infos();
            let mut begin = vk::VideoBeginCodingInfoKHR::default()
                .video_session(self.session.handle)
                .video_session_parameters(self.params.handle)
                .reference_slots(&begin_slots);
            if self.started {
                begin = begin.push_next(&mut rc_info).push_next(&mut h264_rc);
            }

            let settings = &self.settings;
            let started = self.started;
            let dev = &self.gpu.device;
            let video = self.gpu.video.fp();
            let encode = self.gpu.encode.fp();
            let query_pool = self.query_pool;
            let dpb = &self.dpb;
            self.commands
                .run(timeline.semaphore, wait, None, false, |cmd| {
                    input.transition(cmd, vk::ImageLayout::VIDEO_ENCODE_SRC_KHR);
                    dpb.transition(cmd, vk::ImageLayout::VIDEO_ENCODE_DPB_KHR);
                    // SAFETY: recording valid video commands in order on a queue
                    // of the encode family; all referenced structs outlive the call.
                    unsafe {
                        dev.cmd_reset_query_pool(cmd, query_pool, 0, 1);
                        (video.cmd_begin_video_coding_khr)(cmd, &begin);
                        if !started {
                            let mut rc = RateControl::new(settings);
                            let (mut rc_info, mut h264_rc) = rc.infos();
                            let mut quality =
                                vk::VideoEncodeQualityLevelInfoKHR::default().quality_level(0);
                            let control = vk::VideoCodingControlInfoKHR::default()
                                .flags(
                                    vk::VideoCodingControlFlagsKHR::RESET
                                        | vk::VideoCodingControlFlagsKHR::ENCODE_RATE_CONTROL
                                        | vk::VideoCodingControlFlagsKHR::ENCODE_QUALITY_LEVEL,
                                )
                                .push_next(&mut rc_info)
                                .push_next(&mut h264_rc)
                                .push_next(&mut quality);
                            (video.cmd_control_video_coding_khr)(cmd, &control);
                        }
                        dev.cmd_begin_query(cmd, query_pool, 0, vk::QueryControlFlags::empty());
                        (encode.cmd_encode_video_khr)(cmd, &encode_info);
                        dev.cmd_end_query(cmd, query_pool, 0);
                        (video.cmd_end_video_coding_khr)(
                            cmd,
                            &vk::VideoEndCodingInfoKHR::default(),
                        );
                    }
                    Ok(())
                })?;
            Ok::<(), Error>(())
        })?;

        // The DPB and counters describe the picture just recorded; the
        // bitstream itself is collected by `finish`.
        self.slots[setup_slot] = Some(current);
        self.current_ref = Some(setup_slot);
        self.frame_num = (self.frame_num + 1) % (1 << (LOG2_MAX_FRAME_NUM_MINUS4 + 4));
        self.poc += 2;
        self.started = true;
        Ok(PendingEncode { idr })
    }

    /// Wait for a submitted encode and collect its access unit.
    pub(crate) fn finish(&mut self, pending: PendingEncode) -> Result<EncodedPacket> {
        let idr = pending.idr;
        self.commands.wait()?;
        // Feedback for the one query: [offset, bytes written, status].
        let mut results = [[0u32; 3]; 1];
        // SAFETY: the submission completed (fence waited), the pool has one query.
        unsafe {
            self.gpu.device.get_query_pool_results(
                self.query_pool,
                0,
                &mut results,
                vk::QueryResultFlags::WAIT | vk::QueryResultFlags::WITH_STATUS_KHR,
            )?;
        }
        let results = results[0];
        if results[2] == 0 {
            return Err(Error::Unsupported("encode query reported no status".into()));
        }
        let (offset, len) = (results[0] as usize, results[1] as usize);
        let slice = self.bitstream.read(offset, len);
        let mut data = Vec::with_capacity(len + self.sps.len() + self.pps.len() + 4);
        if idr {
            data.extend_from_slice(&self.sps);
            data.extend_from_slice(&self.pps);
        }
        if !has_start_code(&slice[..slice.len().min(5)]) {
            data.extend_from_slice(&[0, 0, 0, 1]);
        }
        data.extend_from_slice(&slice);
        Ok(EncodedPacket {
            keyframe: idr,
            data,
        })
    }
}

/// An encode that has been submitted but not yet read back.
pub(crate) struct PendingEncode {
    idr: bool,
}

/// The CBR rate control state: one layer at the target bitrate and frame
/// rate. Owns the layer array the info structs point into.
struct RateControl {
    h264_layer: vk::VideoEncodeH264RateControlLayerInfoKHR<'static>,
    layers: [vk::VideoEncodeRateControlLayerInfoKHR<'static>; 1],
}

impl RateControl {
    fn new(s: &EncoderSettings) -> Self {
        let h264_layer = vk::VideoEncodeH264RateControlLayerInfoKHR::default();
        let layers = [vk::VideoEncodeRateControlLayerInfoKHR::default()
            .average_bitrate(s.bitrate as u64)
            .max_bitrate(s.bitrate as u64)
            .frame_rate_numerator(s.framerate)
            .frame_rate_denominator(1)];
        Self { h264_layer, layers }
    }

    /// The two structs to chain onto a control or begin info. Both borrow
    /// `self`, which must outlive the command that records them.
    fn infos(
        &mut self,
    ) -> (
        vk::VideoEncodeRateControlInfoKHR<'_>,
        vk::VideoEncodeH264RateControlInfoKHR<'_>,
    ) {
        self.layers[0].p_next =
            (&mut self.h264_layer as *mut vk::VideoEncodeH264RateControlLayerInfoKHR).cast();
        let rc = vk::VideoEncodeRateControlInfoKHR::default()
            .rate_control_mode(vk::VideoEncodeRateControlModeFlagsKHR::CBR)
            .layers(&self.layers)
            .virtual_buffer_size_in_ms(500)
            .initial_virtual_buffer_size_in_ms(250);
        let h264 = vk::VideoEncodeH264RateControlInfoKHR::default()
            .flags(
                vk::VideoEncodeH264RateControlFlagsKHR::REGULAR_GOP
                    | vk::VideoEncodeH264RateControlFlagsKHR::REFERENCE_PATTERN_FLAT,
            )
            .gop_frame_count(60)
            .idr_period(0)
            .consecutive_b_frame_count(0)
            .temporal_layer_count(1);
        (rc, h264)
    }
}

/// The SPS and PPS this encoder emits: High profile, CABAC, one reference,
/// POC type 0, cropped to the display size.
fn std_parameter_sets(
    s: &EncoderSettings,
    max_level: std_video::StdVideoH264LevelIdc,
) -> (
    std_video::StdVideoH264SequenceParameterSet,
    std_video::StdVideoH264PictureParameterSet,
) {
    let (cw, ch) = (s.coded_width(), s.coded_height());
    let crop = cw != s.width || ch != s.height;
    let mut sps_flags = zeroed::<std_video::StdVideoH264SpsFlags>();
    sps_flags.set_direct_8x8_inference_flag(1);
    sps_flags.set_frame_mbs_only_flag(1);
    sps_flags.set_frame_cropping_flag(crop as u32);
    let sps = std_video::StdVideoH264SequenceParameterSet {
        flags: sps_flags,
        profile_idc: std_video::StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_HIGH,
        level_idc: max_level.min(std_video::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_5_1),
        chroma_format_idc:
            std_video::StdVideoH264ChromaFormatIdc_STD_VIDEO_H264_CHROMA_FORMAT_IDC_420,
        seq_parameter_set_id: 0,
        bit_depth_luma_minus8: 0,
        bit_depth_chroma_minus8: 0,
        log2_max_frame_num_minus4: LOG2_MAX_FRAME_NUM_MINUS4,
        pic_order_cnt_type: std_video::StdVideoH264PocType_STD_VIDEO_H264_POC_TYPE_0,
        offset_for_non_ref_pic: 0,
        offset_for_top_to_bottom_field: 0,
        log2_max_pic_order_cnt_lsb_minus4: LOG2_MAX_POC_LSB_MINUS4,
        num_ref_frames_in_pic_order_cnt_cycle: 0,
        max_num_ref_frames: 1,
        reserved1: 0,
        pic_width_in_mbs_minus1: cw / 16 - 1,
        pic_height_in_map_units_minus1: ch / 16 - 1,
        frame_crop_left_offset: 0,
        frame_crop_right_offset: (cw - s.width) / 2,
        frame_crop_top_offset: 0,
        frame_crop_bottom_offset: (ch - s.height) / 2,
        reserved2: 0,
        pOffsetForRefFrame: std::ptr::null(),
        pScalingLists: std::ptr::null(),
        pSequenceParameterSetVui: std::ptr::null(),
    };
    let mut pps_flags = zeroed::<std_video::StdVideoH264PpsFlags>();
    pps_flags.set_entropy_coding_mode_flag(1);
    pps_flags.set_deblocking_filter_control_present_flag(1);
    let pps = std_video::StdVideoH264PictureParameterSet {
        flags: pps_flags,
        seq_parameter_set_id: 0,
        pic_parameter_set_id: 0,
        num_ref_idx_l0_default_active_minus1: 0,
        num_ref_idx_l1_default_active_minus1: 0,
        weighted_bipred_idc:
            std_video::StdVideoH264WeightedBipredIdc_STD_VIDEO_H264_WEIGHTED_BIPRED_IDC_DEFAULT,
        pic_init_qp_minus26: 0,
        pic_init_qs_minus26: 0,
        chroma_qp_index_offset: 0,
        second_chroma_qp_index_offset: 0,
        pScalingLists: std::ptr::null(),
    };
    (sps, pps)
}

impl Drop for H264Encoder {
    fn drop(&mut self) {
        let _ = self.commands.wait();
        // SAFETY: work is complete; fields drop after this in declaration order.
        unsafe { self.gpu.device.destroy_query_pool(self.query_pool, None) };
    }
}
