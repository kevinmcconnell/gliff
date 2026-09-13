//! Bit-level reading of H.264 RBSP data (Exp-Golomb codes and fixed fields).

use crate::{Error, Result};

/// Strip emulation-prevention bytes (`00 00 03` -> `00 00`) from a NAL unit
/// body, giving the raw byte sequence payload the header syntax is read from.
pub fn unescape_rbsp(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &b in nal {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
    out
}

pub struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    pub fn bit(&mut self) -> Result<bool> {
        let byte = self
            .data
            .get(self.pos / 8)
            .ok_or(Error::Bitstream("unexpected end of header"))?;
        let bit = (byte >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        Ok(bit == 1)
    }

    pub fn flag(&mut self) -> Result<u32> {
        Ok(self.bit()? as u32)
    }

    pub fn bits(&mut self, n: u32) -> Result<u32> {
        if n > 32 {
            return Err(Error::Bitstream("field wider than 32 bits"));
        }
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | self.flag()?;
        }
        Ok(v)
    }

    /// Unsigned Exp-Golomb `ue(v)`.
    pub fn ue(&mut self) -> Result<u32> {
        let mut zeros = 0;
        while !self.bit()? {
            zeros += 1;
            if zeros > 31 {
                return Err(Error::Bitstream("Exp-Golomb code too long"));
            }
        }
        let rest = self.bits(zeros)?;
        Ok((1u32 << zeros) - 1 + rest)
    }

    /// Signed Exp-Golomb `se(v)`.
    pub fn se(&mut self) -> Result<i32> {
        let k = self.ue()? as i64;
        Ok(if k % 2 == 1 { (k + 1) / 2 } else { -(k / 2) } as i32)
    }

    pub fn more_rbsp_data(&self) -> bool {
        // True while there are bits before the trailing stop bit.
        let total = self.data.len() * 8;
        if self.pos >= total {
            return false;
        }
        let mut last_one = None;
        for i in (0..self.data.len()).rev() {
            if self.data[i] != 0 {
                last_one = Some(i * 8 + 7 - self.data[i].trailing_zeros() as usize);
                break;
            }
        }
        matches!(last_one, Some(stop) if self.pos < stop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exp_golomb() {
        // 1 -> 0, 010 -> 1, 011 -> 2, 00100 -> 3, 00111 -> 6
        let data = [0b1010_0110, 0b0100_0011, 0b1000_0000];
        let mut r = BitReader::new(&data);
        assert_eq!(r.ue().unwrap(), 0);
        assert_eq!(r.ue().unwrap(), 1);
        assert_eq!(r.ue().unwrap(), 2);
        assert_eq!(r.ue().unwrap(), 3);
        assert_eq!(r.ue().unwrap(), 6);
        let data = [0b0100_1100];
        let mut r = BitReader::new(&data);
        assert_eq!(r.se().unwrap(), 1);
        assert_eq!(r.se().unwrap(), -1);
    }

    #[test]
    fn unescapes() {
        assert_eq!(
            unescape_rbsp(&[0, 0, 3, 1, 0, 0, 3, 0, 0, 3]),
            vec![0, 0, 1, 0, 0, 0, 0]
        );
        assert_eq!(unescape_rbsp(&[0, 0, 3, 3]), vec![0, 0, 3]);
    }
}
