//! Annex B byte-stream helpers: split NAL units and classify them.

pub const NAL_SLICE: u8 = 1;
pub const NAL_IDR: u8 = 5;
pub const NAL_SPS: u8 = 7;
pub const NAL_PPS: u8 = 8;

/// One NAL unit: its type, `nal_ref_idc`, and its bytes starting at the
/// header byte (no start code).
#[derive(Debug, Clone, Copy)]
pub struct Nal<'a> {
    pub nal_type: u8,
    pub ref_idc: u8,
    pub data: &'a [u8],
}

/// Split an Annex B stream at its start codes.
pub fn nal_units(data: &[u8]) -> Vec<Nal<'_>> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    starts
        .iter()
        .enumerate()
        .filter_map(|(n, &body)| {
            // Trailing zero bytes are never part of a NAL unit (the RBSP stop
            // bit and cabac_zero_words both end in a nonzero byte): they are
            // the leading zero of a 4-byte start code or trailing_zero_8bits.
            let mut end = starts.get(n + 1).map(|s| s - 3).unwrap_or(data.len());
            while end > body && data[end - 1] == 0 {
                end -= 1;
            }
            let unit = &data[body..end];
            unit.first().map(|h| Nal {
                nal_type: h & 0x1f,
                ref_idc: (h >> 5) & 3,
                data: unit,
            })
        })
        .collect()
}

pub fn has_start_code(data: &[u8]) -> bool {
    data.windows(3).any(|w| w == [0, 0, 1])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_units() {
        let s = [
            0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 9, 9,
        ];
        let units = nal_units(&s);
        let v: Vec<(u8, u8, usize)> = units
            .iter()
            .map(|n| (n.nal_type, n.ref_idc, n.data.len()))
            .collect();
        assert_eq!(v, vec![(NAL_SPS, 3, 3), (NAL_PPS, 3, 2), (NAL_IDR, 3, 3)]);
    }
}
