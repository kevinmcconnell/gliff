//! Minimal H.264 header parser: the SPS, PPS and slice-header fields a Vulkan
//! decoder must supply. Slice *data* is never parsed; the hardware does that.
//!
//! The parser covers the syntax our own encoder can emit plus the common
//! optional parts (scaling lists, VUI presence), and rejects what it cannot
//! represent (interlace, slice groups, multiple views) with an error rather
//! than guessing.

use super::bits::{unescape_rbsp, BitReader};
use crate::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScalingLists {
    /// Bit i set: list i is present in the stream (0..6 are 4x4, 6.. are 8x8).
    pub present_mask: u16,
    /// Bit i set: list i uses the default matrix (`useDefaultScalingMatrixFlag`).
    pub use_default_mask: u16,
    pub list_4x4: [[u8; 16]; 6],
    pub list_8x8: [[u8; 64]; 6],
}

impl Default for ScalingLists {
    fn default() -> Self {
        Self {
            present_mask: 0,
            use_default_mask: 0,
            list_4x4: [[0; 16]; 6],
            list_8x8: [[0; 64]; 6],
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sps {
    pub profile_idc: u8,
    pub constraint_flags: u8,
    pub level_idc: u8,
    pub sps_id: u8,
    pub chroma_format_idc: u8,
    pub separate_colour_plane: bool,
    pub bit_depth_luma_minus8: u8,
    pub bit_depth_chroma_minus8: u8,
    pub qpprime_y_zero_transform_bypass: bool,
    pub scaling_lists: Option<ScalingLists>,
    pub log2_max_frame_num_minus4: u8,
    pub pic_order_cnt_type: u8,
    pub log2_max_pic_order_cnt_lsb_minus4: u8,
    pub delta_pic_order_always_zero: bool,
    pub offset_for_non_ref_pic: i32,
    pub offset_for_top_to_bottom_field: i32,
    pub offset_for_ref_frame: Vec<i32>,
    pub max_num_ref_frames: u8,
    pub gaps_in_frame_num_allowed: bool,
    pub pic_width_in_mbs_minus1: u32,
    pub pic_height_in_map_units_minus1: u32,
    pub frame_mbs_only: bool,
    pub mb_adaptive_frame_field: bool,
    pub direct_8x8_inference: bool,
    pub frame_cropping: Option<[u32; 4]>,
    pub vui_present: bool,
}

impl Sps {
    pub fn max_frame_num(&self) -> u32 {
        1 << (self.log2_max_frame_num_minus4 + 4)
    }

    pub fn max_poc_lsb(&self) -> u32 {
        1 << (self.log2_max_pic_order_cnt_lsb_minus4 + 4)
    }

    /// Coded (macroblock-aligned) size.
    pub fn coded_size(&self) -> (u32, u32) {
        let mult = if self.frame_mbs_only { 1 } else { 2 };
        (
            self.pic_width_in_mbs_minus1
                .saturating_add(1)
                .saturating_mul(16),
            self.pic_height_in_map_units_minus1
                .saturating_add(1)
                .saturating_mul(16 * mult),
        )
    }

    /// Displayed size after cropping (4:2:0, frame pictures).
    pub fn display_size(&self) -> (u32, u32) {
        let (w, h) = self.coded_size();
        match self.frame_cropping {
            Some([l, r, t, b]) => (
                w.saturating_sub(l.saturating_add(r).saturating_mul(2)),
                h.saturating_sub(t.saturating_add(b).saturating_mul(2)),
            ),
            None => (w, h),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pps {
    pub pps_id: u8,
    pub sps_id: u8,
    pub entropy_coding_mode: bool,
    pub bottom_field_pic_order_in_frame_present: bool,
    pub num_ref_idx_l0_default_active_minus1: u8,
    pub num_ref_idx_l1_default_active_minus1: u8,
    pub weighted_pred: bool,
    pub weighted_bipred_idc: u8,
    pub pic_init_qp_minus26: i8,
    pub pic_init_qs_minus26: i8,
    pub chroma_qp_index_offset: i8,
    pub deblocking_filter_control_present: bool,
    pub constrained_intra_pred: bool,
    pub redundant_pic_cnt_present: bool,
    pub transform_8x8_mode: bool,
    pub scaling_lists: Option<ScalingLists>,
    pub second_chroma_qp_index_offset: i8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SliceType {
    P,
    B,
    I,
}

/// Memory-management control operation from `dec_ref_pic_marking`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mmco {
    pub op: u32,
    pub difference_of_pic_nums_minus1: u32,
    pub long_term_pic_num: u32,
    pub long_term_frame_idx: u32,
    pub max_long_term_frame_idx_plus1: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SliceHeader {
    pub nal_type: u8,
    pub ref_idc: u8,
    pub first_mb_in_slice: u32,
    pub slice_type: Option<SliceType>,
    pub pps_id: u8,
    pub frame_num: u32,
    pub idr_pic_id: u32,
    pub pic_order_cnt_lsb: u32,
    pub delta_pic_order_cnt_bottom: i32,
    pub delta_pic_order_cnt: [i32; 2],
    pub num_ref_idx_l0_active_minus1: u8,
    pub no_output_of_prior_pics: bool,
    pub long_term_reference: bool,
    /// `None` = sliding window; `Some` = adaptive marking with these ops.
    pub mmcos: Option<Vec<Mmco>>,
}

impl SliceHeader {
    pub fn is_idr(&self) -> bool {
        self.nal_type == super::annexb::NAL_IDR
    }

    pub fn is_reference(&self) -> bool {
        self.ref_idc != 0
    }
}

/// An `ue(v)` field with a syntax-defined upper bound; larger values are a
/// malformed (or hostile) stream.
fn ue_max(r: &mut BitReader, max: u32, what: &'static str) -> Result<u32> {
    let v = r.ue()?;
    if v > max {
        tracing::debug!(field = what, value = v, max, "out-of-range header field");
        return Err(Error::Bitstream("header field out of range"));
    }
    Ok(v)
}

fn scaling_list(r: &mut BitReader, size: usize) -> Result<(Vec<u8>, bool)> {
    let mut list = vec![0u8; size];
    let mut last = 8i32;
    let mut next = 8i32;
    let mut use_default = false;
    for (j, v) in list.iter_mut().enumerate() {
        if next != 0 {
            let delta = r.se()?;
            next = ((last as i64 + delta as i64 + 256).rem_euclid(256)) as i32;
            use_default = j == 0 && next == 0;
        }
        *v = if next == 0 { last } else { next } as u8;
        last = *v as i32;
    }
    Ok((list, use_default))
}

fn scaling_lists(r: &mut BitReader, count: usize) -> Result<ScalingLists> {
    let mut out = ScalingLists::default();
    for i in 0..count {
        if r.bit()? {
            out.present_mask |= 1 << i;
            let size = if i < 6 { 16 } else { 64 };
            let (list, default) = scaling_list(r, size)?;
            if default {
                out.use_default_mask |= 1 << i;
            } else if i < 6 {
                out.list_4x4[i].copy_from_slice(&list);
            } else {
                out.list_8x8[i - 6].copy_from_slice(&list);
            }
        }
    }
    Ok(out)
}

/// Parse a sequence parameter set NAL unit (bytes from the header byte).
pub fn parse_sps(nal: &[u8]) -> Result<Sps> {
    let rbsp = unescape_rbsp(nal.get(1..).ok_or(Error::Bitstream("empty SPS"))?);
    let mut r = BitReader::new(&rbsp);
    let mut sps = Sps {
        profile_idc: r.bits(8)? as u8,
        constraint_flags: r.bits(8)? as u8,
        level_idc: r.bits(8)? as u8,
        ..Default::default()
    };
    sps.sps_id = r.ue()? as u8;
    sps.chroma_format_idc = 1;
    if matches!(
        sps.profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        sps.chroma_format_idc = r.ue()? as u8;
        if sps.chroma_format_idc == 3 {
            sps.separate_colour_plane = r.bit()?;
        }
        sps.bit_depth_luma_minus8 = r.ue()? as u8;
        sps.bit_depth_chroma_minus8 = r.ue()? as u8;
        sps.qpprime_y_zero_transform_bypass = r.bit()?;
        if r.bit()? {
            let count = if sps.chroma_format_idc != 3 { 8 } else { 12 };
            sps.scaling_lists = Some(scaling_lists(&mut r, count)?);
        }
    }
    sps.log2_max_frame_num_minus4 = ue_max(&mut r, 12, "log2_max_frame_num_minus4")? as u8;
    sps.pic_order_cnt_type = r.ue()? as u8;
    match sps.pic_order_cnt_type {
        0 => {
            sps.log2_max_pic_order_cnt_lsb_minus4 =
                ue_max(&mut r, 12, "log2_max_pic_order_cnt_lsb_minus4")? as u8
        }
        1 => {
            sps.delta_pic_order_always_zero = r.bit()?;
            sps.offset_for_non_ref_pic = r.se()?;
            sps.offset_for_top_to_bottom_field = r.se()?;
            let n = ue_max(&mut r, 255, "num_ref_frames_in_pic_order_cnt_cycle")?;
            for _ in 0..n {
                sps.offset_for_ref_frame.push(r.se()?);
            }
        }
        2 => {}
        _ => return Err(Error::Bitstream("bad pic_order_cnt_type")),
    }
    sps.max_num_ref_frames = ue_max(&mut r, 16, "max_num_ref_frames")? as u8;
    sps.gaps_in_frame_num_allowed = r.bit()?;
    // 16384 pixels each way is beyond any level; the decoder checks the
    // device limit, this only keeps the arithmetic sane.
    sps.pic_width_in_mbs_minus1 = ue_max(&mut r, 1023, "pic_width_in_mbs_minus1")?;
    sps.pic_height_in_map_units_minus1 = ue_max(&mut r, 1023, "pic_height_in_map_units_minus1")?;
    sps.frame_mbs_only = r.bit()?;
    if !sps.frame_mbs_only {
        sps.mb_adaptive_frame_field = r.bit()?;
    }
    sps.direct_8x8_inference = r.bit()?;
    if r.bit()? {
        let crop = [
            ue_max(&mut r, 8192, "frame_crop_left_offset")?,
            ue_max(&mut r, 8192, "frame_crop_right_offset")?,
            ue_max(&mut r, 8192, "frame_crop_top_offset")?,
            ue_max(&mut r, 8192, "frame_crop_bottom_offset")?,
        ];
        sps.frame_cropping = Some(crop);
    }
    sps.vui_present = r.bit()?;
    if !sps.frame_mbs_only {
        return Err(Error::Bitstream("interlaced streams are not supported"));
    }
    if sps.chroma_format_idc != 1
        || sps.bit_depth_luma_minus8 != 0
        || sps.bit_depth_chroma_minus8 != 0
    {
        return Err(Error::Bitstream("only 8-bit 4:2:0 is supported"));
    }
    Ok(sps)
}

/// Parse a picture parameter set NAL unit. Needs its SPS for the chroma
/// format (scaling list count) and the transform-8x8 tail.
pub fn parse_pps(nal: &[u8], sps_for: impl Fn(u8) -> Option<Sps>) -> Result<Pps> {
    let rbsp = unescape_rbsp(nal.get(1..).ok_or(Error::Bitstream("empty PPS"))?);
    let mut r = BitReader::new(&rbsp);
    let mut pps = Pps {
        pps_id: r.ue()? as u8,
        sps_id: r.ue()? as u8,
        ..Default::default()
    };
    let sps = sps_for(pps.sps_id).ok_or(Error::Bitstream("PPS refers to an unknown SPS"))?;
    pps.entropy_coding_mode = r.bit()?;
    pps.bottom_field_pic_order_in_frame_present = r.bit()?;
    if r.ue()? != 0 {
        return Err(Error::Bitstream("slice groups are not supported"));
    }
    pps.num_ref_idx_l0_default_active_minus1 = r.ue()? as u8;
    pps.num_ref_idx_l1_default_active_minus1 = r.ue()? as u8;
    pps.weighted_pred = r.bit()?;
    pps.weighted_bipred_idc = r.bits(2)? as u8;
    pps.pic_init_qp_minus26 = r.se()? as i8;
    pps.pic_init_qs_minus26 = r.se()? as i8;
    pps.chroma_qp_index_offset = r.se()? as i8;
    pps.deblocking_filter_control_present = r.bit()?;
    pps.constrained_intra_pred = r.bit()?;
    pps.redundant_pic_cnt_present = r.bit()?;
    pps.second_chroma_qp_index_offset = pps.chroma_qp_index_offset;
    if r.more_rbsp_data() {
        pps.transform_8x8_mode = r.bit()?;
        if r.bit()? {
            let count = 6 + if sps.chroma_format_idc != 3 { 2 } else { 6 }
                * pps.transform_8x8_mode as usize;
            pps.scaling_lists = Some(scaling_lists(&mut r, count)?);
        }
        pps.second_chroma_qp_index_offset = r.se()? as i8;
    }
    Ok(pps)
}

/// Parse a slice header up to and including `dec_ref_pic_marking`.
pub fn parse_slice_header(
    nal: &[u8],
    pps_for: impl Fn(u8) -> Option<(Pps, Sps)>,
) -> Result<SliceHeader> {
    let header = *nal.first().ok_or(Error::Bitstream("empty slice"))?;
    let rbsp = unescape_rbsp(&nal[1..]);
    let mut r = BitReader::new(&rbsp);
    let mut sh = SliceHeader {
        nal_type: header & 0x1f,
        ref_idc: (header >> 5) & 3,
        ..Default::default()
    };
    sh.first_mb_in_slice = r.ue()?;
    sh.slice_type = match r.ue()? % 5 {
        0 => Some(SliceType::P),
        1 => Some(SliceType::B),
        2 => Some(SliceType::I),
        _ => None,
    };
    let Some(slice_type) = sh.slice_type else {
        return Err(Error::Bitstream("SP/SI slices are not supported"));
    };
    sh.pps_id = r.ue()? as u8;
    let (pps, sps) =
        pps_for(sh.pps_id).ok_or(Error::Bitstream("slice refers to an unknown PPS"))?;
    if sps.separate_colour_plane {
        r.bits(2)?;
    }
    sh.frame_num = r.bits(sps.log2_max_frame_num_minus4 as u32 + 4)?;
    if !sps.frame_mbs_only && r.bit()? {
        return Err(Error::Bitstream("field pictures are not supported"));
    }
    if sh.is_idr() {
        sh.idr_pic_id = r.ue()?;
    }
    if sps.pic_order_cnt_type == 0 {
        sh.pic_order_cnt_lsb = r.bits(sps.log2_max_pic_order_cnt_lsb_minus4 as u32 + 4)?;
        if pps.bottom_field_pic_order_in_frame_present {
            sh.delta_pic_order_cnt_bottom = r.se()?;
        }
    }
    if sps.pic_order_cnt_type == 1 && !sps.delta_pic_order_always_zero {
        sh.delta_pic_order_cnt[0] = r.se()?;
        if pps.bottom_field_pic_order_in_frame_present {
            sh.delta_pic_order_cnt[1] = r.se()?;
        }
    }
    if pps.redundant_pic_cnt_present {
        r.ue()?;
    }
    if slice_type == SliceType::B {
        r.bit()?; // direct_spatial_mv_pred_flag
    }
    sh.num_ref_idx_l0_active_minus1 = pps.num_ref_idx_l0_default_active_minus1;
    let mut num_l1 = pps.num_ref_idx_l1_default_active_minus1;
    if slice_type != SliceType::I && r.bit()? {
        sh.num_ref_idx_l0_active_minus1 = r.ue()? as u8;
        if slice_type == SliceType::B {
            num_l1 = r.ue()? as u8;
        }
    }
    // ref_pic_list_modification
    if slice_type != SliceType::I {
        skip_ref_pic_list_modification(&mut r)?;
    }
    if slice_type == SliceType::B {
        skip_ref_pic_list_modification(&mut r)?;
    }
    if (pps.weighted_pred && slice_type == SliceType::P)
        || (pps.weighted_bipred_idc == 1 && slice_type == SliceType::B)
    {
        skip_pred_weight_table(
            &mut r,
            &sps,
            sh.num_ref_idx_l0_active_minus1,
            num_l1,
            slice_type == SliceType::B,
        )?;
    }
    if sh.is_reference() {
        if sh.is_idr() {
            sh.no_output_of_prior_pics = r.bit()?;
            sh.long_term_reference = r.bit()?;
        } else if r.bit()? {
            let mut ops = Vec::new();
            loop {
                let op = r.ue()?;
                if op == 0 {
                    break;
                }
                // A conformant stream needs at most a few operations; a long
                // run is slice data misread as a header, or hostile input.
                if ops.len() >= 32 || op > 6 {
                    return Err(Error::Bitstream(
                        "bad memory_management_control_operation list",
                    ));
                }
                let mut m = Mmco {
                    op,
                    difference_of_pic_nums_minus1: 0,
                    long_term_pic_num: 0,
                    long_term_frame_idx: 0,
                    max_long_term_frame_idx_plus1: 0,
                };
                if op == 1 || op == 3 {
                    m.difference_of_pic_nums_minus1 = r.ue()?;
                }
                if op == 2 {
                    m.long_term_pic_num = r.ue()?;
                }
                if op == 3 || op == 6 {
                    m.long_term_frame_idx = r.ue()?;
                }
                if op == 4 {
                    m.max_long_term_frame_idx_plus1 = r.ue()?;
                }
                ops.push(m);
            }
            sh.mmcos = Some(ops);
        }
    }
    Ok(sh)
}

fn skip_ref_pic_list_modification(r: &mut BitReader) -> Result<()> {
    if r.bit()? {
        loop {
            let idc = r.ue()?;
            if idc == 3 {
                break;
            }
            if idc > 5 {
                return Err(Error::Bitstream("bad modification_of_pic_nums_idc"));
            }
            r.ue()?;
        }
    }
    Ok(())
}

fn skip_pred_weight_table(
    r: &mut BitReader,
    sps: &Sps,
    num_l0: u8,
    num_l1: u8,
    b: bool,
) -> Result<()> {
    r.ue()?; // luma_log2_weight_denom
    if sps.chroma_format_idc != 0 {
        r.ue()?;
    }
    for count in [Some(num_l0), b.then_some(num_l1)].into_iter().flatten() {
        for _ in 0..=count {
            if r.bit()? {
                r.se()?;
                r.se()?;
            }
            if sps.chroma_format_idc != 0 && r.bit()? {
                for _ in 0..2 {
                    r.se()?;
                    r.se()?;
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::h264::annexb::{nal_units, NAL_IDR, NAL_PPS, NAL_SLICE, NAL_SPS};

    /// x264 High profile, 64x64, two frames (IDR + P), CABAC, 8x8 transform.
    const STREAM: &[u8] = include_bytes!("testdata/x264-64x64.264");

    fn unit(t: u8) -> &'static [u8] {
        nal_units(STREAM)
            .into_iter()
            .find(|n| n.nal_type == t)
            .unwrap()
            .data
    }

    #[test]
    fn parses_x264_sps_pps() {
        let sps = parse_sps(unit(NAL_SPS)).unwrap();
        assert_eq!(sps.profile_idc, 100);
        assert_eq!(sps.level_idc, 10);
        assert_eq!(sps.chroma_format_idc, 1);
        assert_eq!(sps.coded_size(), (64, 64));
        assert_eq!(sps.display_size(), (64, 64));
        assert_eq!(sps.max_num_ref_frames, 1);
        assert!(sps.frame_mbs_only);
        assert!(sps.vui_present);
        let pps = parse_pps(unit(NAL_PPS), |_| Some(sps.clone())).unwrap();
        assert!(pps.entropy_coding_mode);
        assert!(pps.transform_8x8_mode);
        assert!(pps.deblocking_filter_control_present);
        assert!(pps.scaling_lists.is_none());
    }

    #[test]
    fn parses_slice_headers() {
        let sps = parse_sps(unit(NAL_SPS)).unwrap();
        let pps = parse_pps(unit(NAL_PPS), |_| Some(sps.clone())).unwrap();
        let lookup = |_| Some((pps.clone(), sps.clone()));
        let idr = parse_slice_header(unit(NAL_IDR), lookup).unwrap();
        assert!(idr.is_idr());
        assert_eq!(idr.slice_type, Some(SliceType::I));
        assert_eq!(idr.frame_num, 0);
        assert_eq!(idr.pic_order_cnt_lsb, 0);
        assert!(idr.mmcos.is_none());
        let p = parse_slice_header(unit(NAL_SLICE), lookup).unwrap();
        assert_eq!(p.slice_type, Some(SliceType::P));
        assert!(p.is_reference());
        assert_eq!(p.frame_num, 1);
        // No B frames: x264 picks POC type 2, so no lsb is coded.
        assert_eq!(sps.pic_order_cnt_type, 2);
        assert!(p.mmcos.is_none());
    }

    #[test]
    fn rejects_truncated() {
        assert!(parse_sps(&unit(NAL_SPS)[..4]).is_err());
    }

    /// Hostile headers must error, never panic or allocate wildly: a
    /// High-profile SPS with an absurd `pic_width_in_mbs_minus1`, and one with
    /// `log2_max_frame_num_minus4` beyond the syntax limit.
    #[test]
    fn rejects_out_of_range_fields() {
        // profile 100, constraints 0, level 40, then ue fields:
        // sps_id=0 chroma=1 bd_luma=0 bd_chroma=0 bypass=0 scaling=0
        // log2_max_frame_num_minus4=<n> poc_type=0 log2_max_poc_lsb_minus4=0
        // max_num_ref_frames=1 gaps=0 width_mbs_minus1=<w> ...
        fn sps_with(log2_fn: u32, width_mbs: u32) -> Vec<u8> {
            let mut bits = String::new();
            let ue = |v: u32| {
                let n = (v + 1).ilog2();
                format!("{}{:b}", "0".repeat(n as usize), v + 1)
            };
            for v in [0, 1, 0, 0] {
                bits += &ue(v);
            }
            bits += "00"; // transform bypass, scaling matrix
            bits += &ue(log2_fn);
            bits += &ue(0); // poc type 0
            bits += &ue(0);
            bits += &ue(1);
            bits += "0";
            bits += &ue(width_mbs);
            bits += &ue(10);
            bits += "1"; // frame_mbs_only
            bits += "1"; // direct_8x8
            bits += "0"; // cropping
            bits += "0"; // vui
            bits += "1"; // stop bit
            while !bits.len().is_multiple_of(8) {
                bits += "0";
            }
            let mut out = vec![0x67, 100, 0, 40];
            for chunk in bits.as_bytes().chunks(8) {
                out.push(u8::from_str_radix(std::str::from_utf8(chunk).unwrap(), 2).unwrap());
            }
            out
        }
        assert!(parse_sps(&sps_with(4, 10)).is_ok());
        assert!(parse_sps(&sps_with(250, 10)).is_err());
        assert!(parse_sps(&sps_with(4, 100_000)).is_err());
    }
}
