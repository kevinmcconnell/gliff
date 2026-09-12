//! Tiny Annex B helpers: iterate NAL units, classify them, extract SPS/PPS.

pub const NAL_IDR: u8 = 5;
pub const NAL_SPS: u8 = 7;
pub const NAL_PPS: u8 = 8;
pub const NAL_AUD: u8 = 9;

/// Access unit delimiter for a primary coded picture of any slice type.
pub const AUD: [u8; 6] = [0, 0, 0, 1, 0x09, 0xF0];

/// Yields `(nal_type, payload_including_header)` for each NAL unit.
pub fn nal_units(data: &[u8]) -> impl Iterator<Item = (u8, &[u8])> {
    let starts: Vec<(usize, usize)> = start_codes(data);
    let n = starts.len();
    (0..n).filter_map(move |i| {
        let (_, body_start) = starts[i];
        let end = if i + 1 < n { starts[i + 1].0 } else { data.len() };
        let body = &data[body_start..end];
        body.first().map(|h| (h & 0x1F, body))
    })
}

fn start_codes(data: &[u8]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            let sc_start = if i > 0 && data[i - 1] == 0 { i - 1 } else { i };
            out.push((sc_start, i + 3));
            i += 3;
        } else {
            i += 1;
        }
    }
    out
}

pub fn has_start_code(data: &[u8]) -> bool {
    data.windows(3).any(|w| w == [0, 0, 1])
}

/// Byte offset of each NAL unit's header (the byte after its start code).
pub fn nal_header_offsets(data: &[u8]) -> Vec<usize> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            if i + 3 < data.len() {
                out.push(i + 3);
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    out
}

pub fn contains_idr(data: &[u8]) -> bool {
    nal_units(data).any(|(t, _)| t == NAL_IDR)
}

/// SPS and PPS NAL units with their start codes, in stream order.
pub fn parameter_sets(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for (t, body) in nal_units(data) {
        if t == NAL_SPS || t == NAL_PPS {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(body);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_nal_units() {
        let stream = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 9, 9];
        let units: Vec<(u8, usize)> = nal_units(&stream).map(|(t, b)| (t, b.len())).collect();
        assert_eq!(units, vec![(NAL_SPS, 3), (NAL_PPS, 2), (NAL_IDR, 3)]);
        assert!(contains_idr(&stream));
        assert_eq!(parameter_sets(&stream), vec![0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x68, 3]);
    }
}
