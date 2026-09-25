//! Fused BGRA <-> I420 conversion for the CPU pipeline: BT.709 limited range
//! in 13-bit fixed point, one pass over the frame, with AVX2 row kernels
//! chosen at run time. `gliff_proto::color` and `gliff_proto::chroma` are the
//! floating-point reference these kernels are tested against; the fixed-point
//! result may differ from it by one code value where a sample rounds at .5.

use gliff_proto::chroma::Yuv444;
use rayon::prelude::*;

/// A planar I420 frame, tightly packed: Y is `w*h`, U and V are `w/2*h/2`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct I420 {
    pub width: usize,
    pub height: usize,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
}

impl I420 {
    pub fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            y: vec![0; width * height],
            u: vec![0; width * height / 4],
            v: vec![0; width * height / 4],
        }
    }
}

const FRAC: i32 = 13;
const HALF: i32 = 1 << (FRAC - 1);

// BT.709 limited range, scaled by 2^FRAC. Forward: Y' = 16 + 0.1826 R +
// 0.6142 G + 0.0620 B, and so on, as in `gliff_proto::color`.
const KYR: i16 = 1496;
const KYG: i16 = 5032;
const KYB: i16 = 508;
const KUR: i16 = -824;
const KUG: i16 = -2774;
const KUB: i16 = 3598;
const KVR: i16 = 3598;
const KVG: i16 = -3268;
const KVB: i16 = -330;
// Inverse: R = 1.1644 (Y'-16) + 1.7927 (Cr-128), and so on.
const KY: i16 = 9539;
const KRV: i16 = 14686;
const KGU: i16 = -1747;
const KGV: i16 = -4366;
const KBU: i16 = 17305;

const Y_BIAS: i32 = (16 << FRAC) + HALF;
const C_BIAS: i32 = (128 << FRAC) + HALF;

/// Pack BGRA to I420 with the chroma of each 2x2 block taken from its
/// top-left pixel, exactly as `gliff_proto::chroma::yuv444_to_nv12` does.
/// Width and height must be even; `stride` is bytes per source row.
pub fn bgra_to_i420(bgra: &[u8], stride: usize, width: usize, height: usize) -> I420 {
    assert!(width % 2 == 0 && height % 2 == 0, "dimensions must be even");
    let cw = width / 2;
    let mut out = I420::new(width, height);
    out.y
        .par_chunks_mut(2 * width)
        .zip(out.u.par_chunks_mut(cw))
        .zip(out.v.par_chunks_mut(cw))
        .enumerate()
        .for_each(|(pair, ((y, u), v))| {
            let top = &bgra[2 * pair * stride..][..width * 4];
            let bottom = &bgra[(2 * pair + 1) * stride..][..width * 4];
            let (y_top, y_bottom) = y.split_at_mut(width);
            row_y(top, y_top);
            row_y(bottom, y_bottom);
            row_uv(top, u, v, 2);
        });
    out
}

/// Convert BGRA to planar 4:4:4, for the dual-stream split.
pub fn bgra_to_yuv444(bgra: &[u8], stride: usize, width: usize, height: usize) -> Yuv444 {
    let mut out = Yuv444::new(width, height);
    out.y
        .par_chunks_mut(width)
        .zip(out.u.par_chunks_mut(width))
        .zip(out.v.par_chunks_mut(width))
        .enumerate()
        .for_each(|(row, ((y, u), v))| {
            let line = &bgra[row * stride..][..width * 4];
            row_y(line, y);
            row_uv(line, u, v, 1);
        });
    out
}

/// Split 4:4:4 into the main and auxiliary I420 frames of AVC444, with the
/// plane layout of `gliff_proto::chroma::split_yuv444`.
pub fn split_yuv444(src: &Yuv444) -> (I420, I420) {
    let (w, h) = (src.width, src.height);
    assert!(w % 2 == 0 && h % 2 == 0, "dimensions must be even");
    let (cw, half) = (w / 2, h / 2);
    let mut main = I420::new(w, h);
    let mut aux = I420::new(w, h);
    main.y.copy_from_slice(&src.y);

    // Even rows: the even columns go to main, the odd columns to aux.
    let deinterleave = |line: &[u8], even: &mut [u8], odd: &mut [u8]| {
        for ((pair, e), o) in line.chunks_exact(2).zip(even).zip(odd) {
            *e = pair[0];
            *o = pair[1];
        }
    };
    main.u
        .par_chunks_mut(cw)
        .zip(main.v.par_chunks_mut(cw))
        .zip(aux.u.par_chunks_mut(cw))
        .zip(aux.v.par_chunks_mut(cw))
        .enumerate()
        .for_each(|(by, (((mu, mv), au), av))| {
            let line = 2 * by * w;
            deinterleave(&src.u[line..line + w], mu, au);
            deinterleave(&src.v[line..line + w], mv, av);
        });

    // Odd rows: U in the top half of aux luma, V in the bottom half.
    let (aux_u, aux_v) = aux.y.split_at_mut(half * w);
    aux_u
        .par_chunks_mut(w)
        .zip(aux_v.par_chunks_mut(w))
        .enumerate()
        .for_each(|(y, (u, v))| {
            let line = (2 * y + 1) * w;
            u.copy_from_slice(&src.u[line..line + w]);
            v.copy_from_slice(&src.v[line..line + w]);
        });
    (main, aux)
}

/// Unpack I420 (strides may exceed the width) to BGRA, each chroma sample
/// repeated over its 2x2 block. Width and height must be even.
pub fn i420_to_bgra(
    y: &[u8],
    u: &[u8],
    v: &[u8],
    strides: (usize, usize, usize),
    width: usize,
    height: usize,
) -> Vec<u8> {
    assert!(width % 2 == 0 && height % 2 == 0, "dimensions must be even");
    let (sy, su, sv) = strides;
    let mut out = vec![0u8; width * height * 4];
    out.par_chunks_mut(width * 4)
        .enumerate()
        .for_each(|(row, line)| {
            let cw = width / 2;
            row_bgra(
                &y[row * sy..][..width],
                &u[row / 2 * su..][..cw],
                &v[row / 2 * sv..][..cw],
                line,
                true,
            );
        });
    out
}

/// Convert planar 4:4:4 back to BGRA.
pub fn yuv444_to_bgra(src: &Yuv444) -> Vec<u8> {
    let w = src.width;
    let mut out = vec![0u8; w * src.height * 4];
    out.par_chunks_mut(w * 4)
        .enumerate()
        .for_each(|(row, line)| {
            let at = row * w..row * w + w;
            row_bgra(&src.y[at.clone()], &src.u[at.clone()], &src.v[at], line, false);
        });
    out
}

/// Luma of one row: `y.len()` pixels from `bgra`.
fn row_y(bgra: &[u8], y: &mut [u8]) {
    assert!(bgra.len() >= y.len() * 4);
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx2") {
            // SAFETY: AVX2 was detected on this CPU.
            return unsafe { avx2::row_y(bgra, y) };
        }
    }
    scalar::row_y(bgra, y);
}

/// Chroma of one row: `u.len()` samples, one per `step` pixels of `bgra`.
fn row_uv(bgra: &[u8], u: &mut [u8], v: &mut [u8], step: usize) {
    assert!(step == 1 || step == 2);
    assert!(u.len() == v.len() && bgra.len() >= u.len() * step * 4);
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx2") {
            // SAFETY: AVX2 was detected on this CPU.
            return unsafe { avx2::row_uv(bgra, u, v, step) };
        }
    }
    scalar::row_uv(bgra, u, v, step);
}

/// One row of BGRA from luma and chroma rows. With `upsample`, each chroma
/// sample covers two pixels; `y.len()` must then be even.
fn row_bgra(y: &[u8], u: &[u8], v: &[u8], out: &mut [u8], upsample: bool) {
    let samples = if upsample { y.len() / 2 } else { y.len() };
    assert!(!upsample || y.len() % 2 == 0);
    assert!(out.len() >= y.len() * 4 && u.len() >= samples && v.len() >= samples);
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx2") {
            // SAFETY: AVX2 was detected on this CPU.
            return unsafe { avx2::row_bgra(y, u, v, out, upsample) };
        }
    }
    scalar::row_bgra(y, u, v, out, upsample);
}

fn clamp(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

mod scalar {
    use super::*;

    pub fn row_y(bgra: &[u8], y: &mut [u8]) {
        for (px, y) in bgra.chunks_exact(4).zip(y) {
            let (b, g, r) = (px[0] as i32, px[1] as i32, px[2] as i32);
            *y = clamp((KYR as i32 * r + KYG as i32 * g + KYB as i32 * b + Y_BIAS) >> FRAC);
        }
    }

    pub fn row_uv(bgra: &[u8], u: &mut [u8], v: &mut [u8], step: usize) {
        for (i, (u, v)) in u.iter_mut().zip(v).enumerate() {
            let px = &bgra[i * step * 4..][..4];
            let (b, g, r) = (px[0] as i32, px[1] as i32, px[2] as i32);
            *u = clamp((KUR as i32 * r + KUG as i32 * g + KUB as i32 * b + C_BIAS) >> FRAC);
            *v = clamp((KVR as i32 * r + KVG as i32 * g + KVB as i32 * b + C_BIAS) >> FRAC);
        }
    }

    pub fn row_bgra(y: &[u8], u: &[u8], v: &[u8], out: &mut [u8], upsample: bool) {
        for (i, (px, y)) in out.chunks_exact_mut(4).zip(y).enumerate() {
            let c = if upsample { i / 2 } else { i };
            let yy = *y as i32 - 16;
            let cb = u[c] as i32 - 128;
            let cr = v[c] as i32 - 128;
            let r = (KY as i32 * yy + KRV as i32 * cr + HALF) >> FRAC;
            let g = (KY as i32 * yy + KGU as i32 * cb + KGV as i32 * cr + HALF) >> FRAC;
            let b = (KY as i32 * yy + KBU as i32 * cb + HALF) >> FRAC;
            px.copy_from_slice(&[clamp(b), clamp(g), clamp(r), 255]);
        }
    }
}

/// The AVX2 kernels compute the same fixed-point sums as `scalar`, so their
/// output is identical byte for byte; each hands its tail to `scalar`.
#[cfg(target_arch = "x86_64")]
mod avx2 {
    use std::arch::x86_64::*;

    use super::*;

    /// Eight pixels as two registers of four, one i16 lane per channel.
    ///
    /// # Safety
    /// 32 bytes must be readable at `p`.
    #[target_feature(enable = "avx2")]
    unsafe fn load_8px(p: *const u8) -> (__m256i, __m256i) {
        // SAFETY: the caller guarantees 32 readable bytes.
        let (a, b) = unsafe { (_mm_loadu_si128(p.cast()), _mm_loadu_si128(p.add(16).cast())) };
        (_mm256_cvtepu8_epi16(a), _mm256_cvtepu8_epi16(b))
    }

    /// The even pixels of sixteen, in the same shape as `load_8px`.
    ///
    /// # Safety
    /// 64 bytes must be readable at `p`.
    #[target_feature(enable = "avx2")]
    unsafe fn load_even_8px(p: *const u8) -> (__m256i, __m256i) {
        // SAFETY: the caller guarantees 64 readable bytes.
        let (a, b) = unsafe { (_mm256_loadu_si256(p.cast()), _mm256_loadu_si256(p.add(32).cast())) };
        // Per 128-bit lane: pixels 0 and 2 to the low 8 bytes.
        let pick = _mm256_setr_epi8(
            0, 1, 2, 3, 8, 9, 10, 11, -1, -1, -1, -1, -1, -1, -1, -1, //
            0, 1, 2, 3, 8, 9, 10, 11, -1, -1, -1, -1, -1, -1, -1, -1,
        );
        // 64-bit lanes 0 and 2 hold the picks; gather them into the low half.
        let a = _mm256_permute4x64_epi64::<0b00_00_10_00>(_mm256_shuffle_epi8(a, pick));
        let b = _mm256_permute4x64_epi64::<0b00_00_10_00>(_mm256_shuffle_epi8(b, pick));
        let even = _mm256_permute2x128_si256::<0x20>(a, b);
        (
            _mm256_cvtepu8_epi16(_mm256_castsi256_si128(even)),
            _mm256_cvtepu8_epi16(_mm256_extracti128_si256::<1>(even)),
        )
    }

    /// One channel of eight pixels: the weighted sum plus `bias`, shifted to
    /// an integer per i32 lane, in pixel order.
    #[target_feature(enable = "avx2")]
    fn dot_8px(lo: __m256i, hi: __m256i, coef: __m256i, bias: __m256i) -> __m256i {
        // Per pixel: (B·kB + G·kG), (R·kR + A·0).
        let a = _mm256_madd_epi16(lo, coef);
        let b = _mm256_madd_epi16(hi, coef);
        // hadd works per 128-bit lane, leaving pixels 0 1 4 5 | 2 3 6 7.
        let s = _mm256_hadd_epi32(a, b);
        let s = _mm256_permutevar8x32_epi32(s, _mm256_setr_epi32(0, 1, 4, 5, 2, 3, 6, 7));
        _mm256_srai_epi32::<FRAC>(_mm256_add_epi32(s, bias))
    }

    /// Sixteen i32 lanes (two registers, in order) to 16 bytes, clamped.
    #[target_feature(enable = "avx2")]
    fn pack_16(a: __m256i, b: __m256i) -> __m128i {
        // a0-3 b0-3 | a4-7 b4-7, then a0-3 a4-7 b0-3 b4-7.
        let w = _mm256_packus_epi32(a, b);
        let w = _mm256_permute4x64_epi64::<0b11_01_10_00>(w);
        // Per lane the 8 bytes twice: a0-7 a0-7 | b0-7 b0-7.
        let bytes = _mm256_packus_epi16(w, w);
        let bytes = _mm256_permutevar8x32_epi32(bytes, _mm256_setr_epi32(0, 1, 4, 5, 0, 1, 4, 5));
        _mm256_castsi256_si128(bytes)
    }

    #[target_feature(enable = "avx2")]
    fn coef(b: i16, g: i16, r: i16) -> __m256i {
        _mm256_setr_epi16(b, g, r, 0, b, g, r, 0, b, g, r, 0, b, g, r, 0)
    }

    /// # Safety
    /// Needs AVX2; `bgra` must hold `y.len()` pixels.
    #[target_feature(enable = "avx2")]
    pub unsafe fn row_y(bgra: &[u8], y: &mut [u8]) {
        let k = coef(KYB, KYG, KYR);
        let bias = _mm256_set1_epi32(Y_BIAS);
        let n = y.len() / 16 * 16;
        for i in (0..n).step_by(16) {
            // SAFETY: pixels i..i+16 are in bounds; the store writes y[i..i+16].
            unsafe {
                let p = bgra.as_ptr().add(i * 4);
                let (a0, a1) = load_8px(p);
                let (b0, b1) = load_8px(p.add(32));
                let ya = dot_8px(a0, a1, k, bias);
                let yb = dot_8px(b0, b1, k, bias);
                _mm_storeu_si128(y.as_mut_ptr().add(i).cast(), pack_16(ya, yb));
            }
        }
        scalar::row_y(&bgra[n * 4..], &mut y[n..]);
    }

    /// # Safety
    /// Needs AVX2; `bgra` must hold `u.len() * step` pixels, `step` 1 or 2.
    #[target_feature(enable = "avx2")]
    pub unsafe fn row_uv(bgra: &[u8], u: &mut [u8], v: &mut [u8], step: usize) {
        let ku = coef(KUB, KUG, KUR);
        let kv = coef(KVB, KVG, KVR);
        let bias = _mm256_set1_epi32(C_BIAS);
        let n = u.len() / 16 * 16;
        for i in (0..n).step_by(16) {
            // SAFETY: pixels i*step..(i+16)*step are in bounds; the stores
            // write u[i..i+16] and v[i..i+16].
            unsafe {
                let p = bgra.as_ptr().add(i * step * 4);
                let ((a0, a1), (b0, b1)) = if step == 1 {
                    (load_8px(p), load_8px(p.add(32)))
                } else {
                    (load_even_8px(p), load_even_8px(p.add(64)))
                };
                let ua = dot_8px(a0, a1, ku, bias);
                let ub = dot_8px(b0, b1, ku, bias);
                _mm_storeu_si128(u.as_mut_ptr().add(i).cast(), pack_16(ua, ub));
                let va = dot_8px(a0, a1, kv, bias);
                let vb = dot_8px(b0, b1, kv, bias);
                _mm_storeu_si128(v.as_mut_ptr().add(i).cast(), pack_16(va, vb));
            }
        }
        scalar::row_uv(&bgra[n * step * 4..], &mut u[n..], &mut v[n..], step);
    }

    /// Eight chroma samples as i16, centred on zero.
    #[target_feature(enable = "avx2")]
    fn chroma_8(samples: &[u8], upsample: bool) -> __m128i {
        let raw = if upsample {
            let four = _mm_cvtsi32_si128(u32::from_ne_bytes(samples[..4].try_into().unwrap()) as i32);
            _mm_unpacklo_epi8(four, four)
        } else {
            _mm_cvtsi64_si128(u64::from_ne_bytes(samples[..8].try_into().unwrap()) as i64)
        };
        _mm_sub_epi16(_mm_cvtepu8_epi16(raw), _mm_set1_epi16(128))
    }

    /// # Safety
    /// Needs AVX2; `out` must hold `y.len()` pixels and `u`, `v` the samples
    /// for them (half as many with `upsample`, `y.len()` even).
    #[target_feature(enable = "avx2")]
    pub unsafe fn row_bgra(y: &[u8], u: &[u8], v: &[u8], out: &mut [u8], upsample: bool) {
        let kr = _mm256_setr_epi16(KY, KRV, KY, KRV, KY, KRV, KY, KRV, KY, KRV, KY, KRV, KY, KRV, KY, KRV);
        let kb = _mm256_setr_epi16(KY, KBU, KY, KBU, KY, KBU, KY, KBU, KY, KBU, KY, KBU, KY, KBU, KY, KBU);
        let kg1 = _mm256_setr_epi16(KY, KGU, KY, KGU, KY, KGU, KY, KGU, KY, KGU, KY, KGU, KY, KGU, KY, KGU);
        let kg2 = _mm256_setr_epi16(0, KGV, 0, KGV, 0, KGV, 0, KGV, 0, KGV, 0, KGV, 0, KGV, 0, KGV);
        let half = _mm256_set1_epi32(HALF);
        let alpha = _mm256_set1_epi32(255);
        // Per lane: b0-3 g0-3 r0-3 a0-3 to b0 g0 r0 a0 b1 g1 r1 a1 ...
        let interleave = _mm256_setr_epi8(
            0, 4, 8, 12, 1, 5, 9, 13, 2, 6, 10, 14, 3, 7, 11, 15, //
            0, 4, 8, 12, 1, 5, 9, 13, 2, 6, 10, 14, 3, 7, 11, 15,
        );
        let n = y.len() / 8 * 8;
        let per = if upsample { 2 } else { 1 };
        for i in (0..n).step_by(8) {
            let yy = _mm_cvtsi64_si128(u64::from_ne_bytes(y[i..i + 8].try_into().unwrap()) as i64);
            let yy = _mm_sub_epi16(_mm_cvtepu8_epi16(yy), _mm_set1_epi16(16));
            let cb = chroma_8(&u[i / per..], upsample);
            let cr = chroma_8(&v[i / per..], upsample);
            let ycb = _mm256_set_m128i(_mm_unpackhi_epi16(yy, cb), _mm_unpacklo_epi16(yy, cb));
            let ycr = _mm256_set_m128i(_mm_unpackhi_epi16(yy, cr), _mm_unpacklo_epi16(yy, cr));
            let r = _mm256_srai_epi32::<FRAC>(_mm256_add_epi32(_mm256_madd_epi16(ycr, kr), half));
            let b = _mm256_srai_epi32::<FRAC>(_mm256_add_epi32(_mm256_madd_epi16(ycb, kb), half));
            let g = _mm256_add_epi32(_mm256_madd_epi16(ycb, kg1), _mm256_madd_epi16(ycr, kg2));
            let g = _mm256_srai_epi32::<FRAC>(_mm256_add_epi32(g, half));
            // b0-3 g0-3 | b4-7 g4-7 and r0-3 a0-3 | r4-7 a4-7 as u16, then
            // per lane b g r a of four pixels as bytes.
            let bg = _mm256_packus_epi32(b, g);
            let ra = _mm256_packus_epi32(r, alpha);
            let px = _mm256_shuffle_epi8(_mm256_packus_epi16(bg, ra), interleave);
            // SAFETY: out holds y.len() pixels, so bytes i*4..i*4+32 are in bounds.
            unsafe { _mm256_storeu_si256(out.as_mut_ptr().add(i * 4).cast(), px) };
        }
        scalar::row_bgra(&y[n..], &u[n / per..], &v[n / per..], &mut out[n * 4..], upsample);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gliff_proto::chroma::{nv12_to_yuv444, split_yuv444 as split_nv12, yuv444_to_nv12};
    use gliff_proto::color;

    struct Rng(u32);

    impl Rng {
        fn next(&mut self) -> u8 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 17;
            self.0 ^= self.0 << 5;
            (self.0 >> 8) as u8
        }
        fn bytes(&mut self, n: usize) -> Vec<u8> {
            (0..n).map(|_| self.next()).collect()
        }
    }

    const SIZES: [(usize, usize); 7] = [(2, 2), (6, 4), (16, 2), (34, 6), (66, 4), (100, 2), (640, 4)];

    fn max_diff(a: &[u8], b: &[u8]) -> u8 {
        assert_eq!(a.len(), b.len());
        a.iter().zip(b).map(|(x, y)| x.abs_diff(*y)).max().unwrap_or(0)
    }

    fn i420_from(nv12: &gliff_proto::chroma::Nv12) -> I420 {
        let mut out = I420::new(nv12.width, nv12.height);
        out.y.copy_from_slice(&nv12.y);
        for (i, pair) in nv12.uv.chunks_exact(2).enumerate() {
            out.u[i] = pair[0];
            out.v[i] = pair[1];
        }
        out
    }

    #[test]
    fn bgra_to_i420_matches_the_reference_within_one() {
        let mut rng = Rng(7);
        for (w, h) in SIZES {
            let bgra = rng.bytes(w * h * 4);
            let got = bgra_to_i420(&bgra, w * 4, w, h);
            let want = i420_from(&yuv444_to_nv12(&color::bgra_to_yuv444(&bgra, w * 4, w, h)));
            assert!(max_diff(&got.y, &want.y) <= 1, "luma at {w}x{h}");
            assert!(max_diff(&got.u, &want.u) <= 1, "U at {w}x{h}");
            assert!(max_diff(&got.v, &want.v) <= 1, "V at {w}x{h}");
        }
    }

    #[test]
    fn bgra_to_yuv444_matches_the_reference_within_one() {
        let mut rng = Rng(11);
        for (w, h) in SIZES {
            let bgra = rng.bytes(w * h * 4);
            let got = bgra_to_yuv444(&bgra, w * 4, w, h);
            let want = color::bgra_to_yuv444(&bgra, w * 4, w, h);
            assert!(max_diff(&got.y, &want.y) <= 1, "luma at {w}x{h}");
            assert!(max_diff(&got.u, &want.u) <= 1, "U at {w}x{h}");
            assert!(max_diff(&got.v, &want.v) <= 1, "V at {w}x{h}");
        }
    }

    #[test]
    fn split_matches_the_reference_layout() {
        let mut rng = Rng(13);
        for (w, h) in SIZES {
            let mut src = Yuv444::new(w, h);
            src.y = rng.bytes(w * h);
            src.u = rng.bytes(w * h);
            src.v = rng.bytes(w * h);
            let (main, aux) = split_yuv444(&src);
            let (want_main, want_aux) = split_nv12(&src);
            assert_eq!(main, i420_from(&want_main), "main at {w}x{h}");
            assert_eq!(aux, i420_from(&want_aux), "aux at {w}x{h}");
        }
    }

    #[test]
    fn i420_to_bgra_matches_the_reference_within_one() {
        let mut rng = Rng(17);
        for (w, h) in SIZES {
            let (sy, sc) = (w + 6, w / 2 + 2);
            let y = rng.bytes(sy * h);
            let u = rng.bytes(sc * h / 2);
            let v = rng.bytes(sc * h / 2);
            let got = i420_to_bgra(&y, &u, &v, (sy, sc, sc), w, h);
            let mut nv12 = gliff_proto::chroma::Nv12::new(w, h);
            for row in 0..h {
                nv12.y[row * w..row * w + w].copy_from_slice(&y[row * sy..row * sy + w]);
            }
            for row in 0..h / 2 {
                for col in 0..w / 2 {
                    nv12.uv[row * w + 2 * col] = u[row * sc + col];
                    nv12.uv[row * w + 2 * col + 1] = v[row * sc + col];
                }
            }
            let want = color::yuv444_to_bgra(&nv12_to_yuv444(&nv12));
            assert!(max_diff(&got, &want) <= 1, "at {w}x{h}");
        }
    }

    #[test]
    fn yuv444_to_bgra_matches_the_reference_within_one() {
        let mut rng = Rng(19);
        for (w, h) in SIZES {
            let mut src = Yuv444::new(w, h);
            src.y = rng.bytes(w * h);
            src.u = rng.bytes(w * h);
            src.v = rng.bytes(w * h);
            assert!(max_diff(&yuv444_to_bgra(&src), &color::yuv444_to_bgra(&src)) <= 1, "at {w}x{h}");
        }
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn avx2_rows_equal_scalar_rows_exactly() {
        if !is_x86_feature_detected!("avx2") {
            return;
        }
        let mut rng = Rng(23);
        for w in [2, 8, 16, 18, 32, 34, 50, 64, 100, 130] {
            let bgra = rng.bytes(w * 4);
            let (mut y_s, mut y_v) = (vec![0; w], vec![0; w]);
            scalar::row_y(&bgra, &mut y_s);
            // SAFETY: AVX2 was detected above.
            unsafe { avx2::row_y(&bgra, &mut y_v) };
            assert_eq!(y_s, y_v, "luma at width {w}");
            for step in [1, 2] {
                let n = w / step;
                let (mut u_s, mut v_s, mut u_v, mut v_v) = (vec![0; n], vec![0; n], vec![0; n], vec![0; n]);
                scalar::row_uv(&bgra, &mut u_s, &mut v_s, step);
                // SAFETY: AVX2 was detected above.
                unsafe { avx2::row_uv(&bgra, &mut u_v, &mut v_v, step) };
                assert_eq!((u_s, v_s), (u_v, v_v), "chroma at width {w} step {step}");
            }
            let (y, u, v) = (rng.bytes(w), rng.bytes(w), rng.bytes(w));
            for upsample in [false, true] {
                let (mut out_s, mut out_v) = (vec![0; w * 4], vec![0; w * 4]);
                scalar::row_bgra(&y, &u, &v, &mut out_s, upsample);
                // SAFETY: AVX2 was detected above.
                unsafe { avx2::row_bgra(&y, &u, &v, &mut out_v, upsample) };
                assert_eq!(out_s, out_v, "bgra at width {w} upsample {upsample}");
            }
        }
    }
}
