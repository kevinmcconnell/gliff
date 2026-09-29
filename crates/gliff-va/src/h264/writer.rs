//! Bit-level writing of H.264 headers: the SPS, PPS and slice header the
//! encoder hands the driver as packed headers and puts on the wire.

use super::parser::{Pps, SliceHeader, SliceType, Sps};

/// Accumulates RBSP bits most-significant first.
#[derive(Debug, Default, Clone)]
pub struct BitWriter {
    bytes: Vec<u8>,
    /// Bits used in the last byte, 0..8; 0 means the last byte is full.
    partial: u32,
}

impl BitWriter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn bit(&mut self, v: bool) {
        if self.partial == 0 {
            self.bytes.push(0);
        }
        if v {
            let last = self.bytes.len() - 1;
            self.bytes[last] |= 1 << (7 - self.partial);
        }
        self.partial = (self.partial + 1) % 8;
    }

    pub fn flag(&mut self, v: bool) {
        self.bit(v);
    }

    /// `u(n)`: the low `n` bits of `v`, most significant first.
    pub fn u(&mut self, n: u32, v: u32) {
        for i in (0..n).rev() {
            self.bit((v >> i) & 1 == 1);
        }
    }

    /// Unsigned Exp-Golomb `ue(v)`.
    pub fn ue(&mut self, v: u32) {
        let x = v as u64 + 1;
        let len = 64 - x.leading_zeros();
        self.u(len - 1, 0);
        for i in (0..len).rev() {
            self.bit((x >> i) & 1 == 1);
        }
    }

    /// Signed Exp-Golomb `se(v)`.
    pub fn se(&mut self, v: i32) {
        let k = if v > 0 {
            2 * v as u32 - 1
        } else {
            (-(v as i64) * 2) as u32
        };
        self.ue(k);
    }

    pub fn bit_len(&self) -> usize {
        self.bytes.len() * 8
            - if self.partial == 0 {
                0
            } else {
                8 - self.partial as usize
            }
    }

    /// `rbsp_trailing_bits()`: the stop bit and zero padding to a byte.
    pub fn trailing_bits(&mut self) {
        self.bit(true);
        while self.partial != 0 {
            self.bit(false);
        }
    }

    /// The bits written so far, zero-padded to whole bytes, unescaped.
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

/// Insert emulation-prevention bytes so no `00 00 0x` (x <= 3) appears.
pub fn escape_rbsp(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len() + raw.len() / 64 + 4);
    let mut zeros = 0;
    for &b in raw {
        if zeros >= 2 && b <= 3 {
            out.push(3);
            zeros = 0;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
    out
}

/// A complete NAL unit (no start code): header byte plus escaped payload.
pub fn nal_unit(ref_idc: u8, nal_type: u8, rbsp: &[u8]) -> Vec<u8> {
    let mut out = vec![(ref_idc & 3) << 5 | (nal_type & 0x1f)];
    out.extend(escape_rbsp(rbsp));
    out
}

fn scaling_lists_unsupported(present: bool) {
    assert!(!present, "the writer does not emit scaling lists");
}

/// The SPS payload (RBSP with trailing bits, unescaped). Scaling lists and
/// VUI are not written; POC types 0 and 2 are supported.
pub fn write_sps(sps: &Sps) -> Vec<u8> {
    scaling_lists_unsupported(sps.scaling_lists.is_some());
    assert!(!sps.vui_present, "the writer does not emit VUI");
    let mut w = BitWriter::new();
    w.u(8, sps.profile_idc as u32);
    w.u(8, sps.constraint_flags as u32);
    w.u(8, sps.level_idc as u32);
    w.ue(sps.sps_id as u32);
    if matches!(
        sps.profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        w.ue(sps.chroma_format_idc as u32);
        if sps.chroma_format_idc == 3 {
            w.flag(sps.separate_colour_plane);
        }
        w.ue(sps.bit_depth_luma_minus8 as u32);
        w.ue(sps.bit_depth_chroma_minus8 as u32);
        w.flag(sps.qpprime_y_zero_transform_bypass);
        w.flag(false); // seq_scaling_matrix_present_flag
    }
    w.ue(sps.log2_max_frame_num_minus4 as u32);
    w.ue(sps.pic_order_cnt_type as u32);
    match sps.pic_order_cnt_type {
        0 => w.ue(sps.log2_max_pic_order_cnt_lsb_minus4 as u32),
        1 => {
            w.flag(sps.delta_pic_order_always_zero);
            w.se(sps.offset_for_non_ref_pic);
            w.se(sps.offset_for_top_to_bottom_field);
            w.ue(sps.offset_for_ref_frame.len() as u32);
            for &o in &sps.offset_for_ref_frame {
                w.se(o);
            }
        }
        _ => {}
    }
    w.ue(sps.max_num_ref_frames as u32);
    w.flag(sps.gaps_in_frame_num_allowed);
    w.ue(sps.pic_width_in_mbs_minus1);
    w.ue(sps.pic_height_in_map_units_minus1);
    w.flag(sps.frame_mbs_only);
    if !sps.frame_mbs_only {
        w.flag(sps.mb_adaptive_frame_field);
    }
    w.flag(sps.direct_8x8_inference);
    match sps.frame_cropping {
        Some(crop) => {
            w.flag(true);
            for c in crop {
                w.ue(c);
            }
        }
        None => w.flag(false),
    }
    w.flag(false); // vui_parameters_present_flag
    w.trailing_bits();
    w.into_bytes()
}

/// The PPS payload (RBSP with trailing bits, unescaped).
pub fn write_pps(pps: &Pps) -> Vec<u8> {
    scaling_lists_unsupported(pps.scaling_lists.is_some());
    let mut w = BitWriter::new();
    w.ue(pps.pps_id as u32);
    w.ue(pps.sps_id as u32);
    w.flag(pps.entropy_coding_mode);
    w.flag(pps.bottom_field_pic_order_in_frame_present);
    w.ue(0); // num_slice_groups_minus1
    w.ue(pps.num_ref_idx_l0_default_active_minus1 as u32);
    w.ue(pps.num_ref_idx_l1_default_active_minus1 as u32);
    w.flag(pps.weighted_pred);
    w.u(2, pps.weighted_bipred_idc as u32);
    w.se(pps.pic_init_qp_minus26 as i32);
    w.se(pps.pic_init_qs_minus26 as i32);
    w.se(pps.chroma_qp_index_offset as i32);
    w.flag(pps.deblocking_filter_control_present);
    w.flag(pps.constrained_intra_pred);
    w.flag(pps.redundant_pic_cnt_present);
    if pps.transform_8x8_mode || pps.second_chroma_qp_index_offset != pps.chroma_qp_index_offset {
        w.flag(pps.transform_8x8_mode);
        w.flag(false); // pic_scaling_matrix_present_flag
        w.se(pps.second_chroma_qp_index_offset as i32);
    }
    w.trailing_bits();
    w.into_bytes()
}

/// The slice header bits after the NAL header byte, unescaped and without
/// trailing bits: `slice_data()` follows at `bit_len`. Frame pictures with
/// POC type 0 or 2, I and P slices, no weighted prediction.
pub fn write_slice_header(sh: &SliceHeader, sps: &Sps, pps: &Pps) -> BitWriter {
    let slice_type = sh.slice_type.expect("slice type");
    assert!(sps.frame_mbs_only, "frame pictures only");
    assert!(slice_type != SliceType::B, "B slices are not written");
    assert!(
        sh.pred_weights.is_none(),
        "weighted prediction is not written"
    );
    let mut w = BitWriter::new();
    w.ue(sh.first_mb_in_slice);
    w.ue(match slice_type {
        SliceType::P => 0,
        SliceType::B => 1,
        SliceType::I => 2,
    });
    w.ue(sh.pps_id as u32);
    if sps.separate_colour_plane {
        w.u(2, 0);
    }
    w.u(sps.log2_max_frame_num_minus4 as u32 + 4, sh.frame_num);
    if sh.is_idr() {
        w.ue(sh.idr_pic_id);
    }
    if sps.pic_order_cnt_type == 0 {
        w.u(
            sps.log2_max_pic_order_cnt_lsb_minus4 as u32 + 4,
            sh.pic_order_cnt_lsb,
        );
        if pps.bottom_field_pic_order_in_frame_present {
            w.se(sh.delta_pic_order_cnt_bottom);
        }
    }
    if sps.pic_order_cnt_type == 1 && !sps.delta_pic_order_always_zero {
        w.se(sh.delta_pic_order_cnt[0]);
        if pps.bottom_field_pic_order_in_frame_present {
            w.se(sh.delta_pic_order_cnt[1]);
        }
    }
    if pps.redundant_pic_cnt_present {
        w.ue(0);
    }
    if slice_type == SliceType::P {
        w.flag(sh.num_ref_idx_active_override);
        if sh.num_ref_idx_active_override {
            w.ue(sh.num_ref_idx_l0_active_minus1 as u32);
        }
        // ref_pic_list_modification
        w.flag(!sh.ref_list_mods_l0.is_empty());
        if !sh.ref_list_mods_l0.is_empty() {
            for m in &sh.ref_list_mods_l0 {
                w.ue(m.idc);
                w.ue(m.value);
            }
            w.ue(3);
        }
    }
    if sh.is_reference() {
        if sh.is_idr() {
            w.flag(sh.no_output_of_prior_pics);
            w.flag(sh.long_term_reference);
        } else {
            match &sh.mmcos {
                None => w.flag(false),
                Some(ops) => {
                    w.flag(true);
                    for m in ops {
                        w.ue(m.op);
                        if m.op == 1 || m.op == 3 {
                            w.ue(m.difference_of_pic_nums_minus1);
                        }
                        if m.op == 2 {
                            w.ue(m.long_term_pic_num);
                        }
                        if m.op == 3 || m.op == 6 {
                            w.ue(m.long_term_frame_idx);
                        }
                        if m.op == 4 {
                            w.ue(m.max_long_term_frame_idx_plus1);
                        }
                    }
                    w.ue(0);
                }
            }
        }
    }
    if pps.entropy_coding_mode && slice_type != SliceType::I {
        w.ue(sh.cabac_init_idc as u32);
    }
    w.se(sh.slice_qp_delta as i32);
    if pps.deblocking_filter_control_present {
        w.ue(sh.disable_deblocking_filter_idc as u32);
        if sh.disable_deblocking_filter_idc != 1 {
            w.se(sh.slice_alpha_c0_offset_div2 as i32);
            w.se(sh.slice_beta_offset_div2 as i32);
        }
    }
    w
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::h264::annexb::{NAL_IDR, NAL_PPS, NAL_SLICE, NAL_SPS};
    use crate::h264::bits::{unescape_rbsp, BitReader};
    use crate::h264::parser::{parse_pps, parse_slice_header, parse_sps, Mmco};

    #[test]
    fn exp_golomb_round_trip() {
        let mut w = BitWriter::new();
        let values = [0u32, 1, 2, 3, 6, 7, 8, 255, 256, 65535, 1 << 20];
        for &v in &values {
            w.ue(v);
        }
        let signed = [0i32, 1, -1, 2, -2, 17, -17, 1000, -1000];
        for &v in &signed {
            w.se(v);
        }
        w.trailing_bits();
        let bytes = w.into_bytes();
        let mut r = BitReader::new(&bytes);
        for &v in &values {
            assert_eq!(r.ue().unwrap(), v);
        }
        for &v in &signed {
            assert_eq!(r.se().unwrap(), v);
        }
    }

    #[test]
    fn escaping_inverts_unescaping() {
        let raw = [0, 0, 0, 1, 0, 0, 2, 0, 0, 3, 0, 0, 4, 5, 0, 0];
        let esc = escape_rbsp(&raw);
        assert_eq!(
            esc,
            [0, 0, 3, 0, 1, 0, 0, 3, 2, 0, 0, 3, 3, 0, 0, 4, 5, 0, 0]
        );
        assert_eq!(unescape_rbsp(&esc), raw);
    }

    fn sample_sps() -> Sps {
        Sps {
            profile_idc: 100,
            constraint_flags: 0,
            level_idc: 51,
            chroma_format_idc: 1,
            log2_max_frame_num_minus4: 12,
            pic_order_cnt_type: 0,
            log2_max_pic_order_cnt_lsb_minus4: 12,
            max_num_ref_frames: 1,
            pic_width_in_mbs_minus1: 117,
            pic_height_in_map_units_minus1: 66,
            frame_mbs_only: true,
            direct_8x8_inference: true,
            frame_cropping: Some([0, 0, 0, 8]),
            ..Default::default()
        }
    }

    fn sample_pps() -> Pps {
        Pps {
            entropy_coding_mode: true,
            deblocking_filter_control_present: true,
            transform_8x8_mode: true,
            ..Default::default()
        }
    }

    #[test]
    fn sps_and_pps_round_trip() {
        let sps = sample_sps();
        let nal = nal_unit(3, NAL_SPS, &write_sps(&sps));
        assert_eq!(nal[0], 0x67);
        assert_eq!(parse_sps(&nal).unwrap(), sps);
        let pps = sample_pps();
        let nal = nal_unit(3, NAL_PPS, &write_pps(&pps));
        assert_eq!(nal[0], 0x68);
        assert_eq!(parse_pps(&nal, |_| Some(sps.clone())).unwrap(), pps);
        let plain = Pps::default();
        let nal = nal_unit(3, NAL_PPS, &write_pps(&plain));
        assert_eq!(parse_pps(&nal, |_| Some(sps.clone())).unwrap(), plain);
    }

    #[test]
    fn slice_headers_round_trip() {
        let (sps, pps) = (sample_sps(), sample_pps());
        let lookup = |_| Some((pps.clone(), sps.clone()));
        let idr = SliceHeader {
            nal_type: NAL_IDR,
            ref_idc: 3,
            slice_type: Some(SliceType::I),
            idr_pic_id: 5,
            slice_qp_delta: -3,
            disable_deblocking_filter_idc: 1,
            ..Default::default()
        };
        let w = write_slice_header(&idr, &sps, &pps);
        let bits = w.bit_len() as u32;
        let mut w = w;
        w.trailing_bits();
        let nal = nal_unit(3, NAL_IDR, &w.into_bytes());
        let parsed = parse_slice_header(&nal, lookup).unwrap();
        assert_eq!(parsed.header_bits, bits);
        assert_eq!(
            parsed,
            SliceHeader {
                header_bits: bits,
                ..idr
            }
        );
        let p = SliceHeader {
            nal_type: NAL_SLICE,
            ref_idc: 2,
            slice_type: Some(SliceType::P),
            frame_num: 7,
            pic_order_cnt_lsb: 14,
            cabac_init_idc: 1,
            slice_qp_delta: 4,
            disable_deblocking_filter_idc: 0,
            slice_alpha_c0_offset_div2: -1,
            slice_beta_offset_div2: 2,
            mmcos: Some(vec![Mmco {
                op: 1,
                difference_of_pic_nums_minus1: 0,
                long_term_pic_num: 0,
                long_term_frame_idx: 0,
                max_long_term_frame_idx_plus1: 0,
            }]),
            ..Default::default()
        };
        let mut w = write_slice_header(&p, &sps, &pps);
        let bits = w.bit_len() as u32;
        w.trailing_bits();
        let nal = nal_unit(2, NAL_SLICE, &w.into_bytes());
        let parsed = parse_slice_header(&nal, lookup).unwrap();
        assert_eq!(
            parsed,
            SliceHeader {
                header_bits: bits,
                ..p
            }
        );
    }
}
