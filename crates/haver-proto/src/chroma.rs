//! Full-resolution 4:4:4 <-> two 4:2:0 (NV12) frames, the AVC444 technique.
//!
//! We own both ends, so unlike MS-RDPEGFX AVC444 v1 we do not average the main
//! chroma or apply a reverse filter: the main plane keeps the exact sample at
//! the even position and recombination is bit-exact. Width and height must be
//! even. Layout, per plane (all 8-bit):
//!
//! - main.Y = Y444
//! - main.UV (NV12 interleave): U = U444[2y][2x], V = V444[2y][2x]
//! - aux.Y: top half rows carry U444 odd rows, bottom half V444 odd rows
//! - aux.UV: U = U444[2y][2x+1], V = V444[2y][2x+1]
//!
//! The two frames together carry exactly the 2*W*H chroma samples of 4:4:4.

/// Planar 4:4:4, one byte per sample, tightly packed (stride == width).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Yuv444 {
    pub width: usize,
    pub height: usize,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
}

impl Yuv444 {
    pub fn new(width: usize, height: usize) -> Self {
        Self { width, height, y: vec![0; width * height], u: vec![0; width * height], v: vec![0; width * height] }
    }
}

/// One NV12 plane pair, tightly packed: Y is w*h, UV is w*(h/2) interleaved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Nv12 {
    pub width: usize,
    pub height: usize,
    pub y: Vec<u8>,
    pub uv: Vec<u8>,
}

impl Nv12 {
    pub fn new(width: usize, height: usize) -> Self {
        Self { width, height, y: vec![0; width * height], uv: vec![0; width * height / 2] }
    }
}

/// Split 4:4:4 into a main and auxiliary NV12 frame.
pub fn split_yuv444(src: &Yuv444) -> (Nv12, Nv12) {
    let (w, h) = (src.width, src.height);
    assert!(w % 2 == 0 && h % 2 == 0, "dimensions must be even");
    let mut main = Nv12::new(w, h);
    let mut aux = Nv12::new(w, h);

    main.y.copy_from_slice(&src.y);

    // main chroma: sample at (2y, 2x)
    for by in 0..h / 2 {
        for bx in 0..w / 2 {
            let sy = 2 * by;
            let sx = 2 * bx;
            let dst = by * w + bx * 2;
            main.uv[dst] = src.u[sy * w + sx];
            main.uv[dst + 1] = src.v[sy * w + sx];
        }
    }

    // aux luma: U odd rows in the top half, V odd rows in the bottom half.
    let half = h / 2;
    for y in 0..half {
        let u_src = (2 * y + 1) * w;
        aux.y[y * w..y * w + w].copy_from_slice(&src.u[u_src..u_src + w]);
        let v_src = (2 * y + 1) * w;
        aux.y[(half + y) * w..(half + y) * w + w].copy_from_slice(&src.v[v_src..v_src + w]);
    }

    // aux chroma: the odd column of even rows, U and V.
    for by in 0..h / 2 {
        for bx in 0..w / 2 {
            let sy = 2 * by;
            let sx = 2 * bx + 1;
            let dst = by * w + bx * 2;
            aux.uv[dst] = src.u[sy * w + sx];
            aux.uv[dst + 1] = src.v[sy * w + sx];
        }
    }

    (main, aux)
}

/// Recombine a main and auxiliary NV12 frame into 4:4:4. Inverse of `split_yuv444`.
pub fn recombine_yuv444(main: &Nv12, aux: &Nv12) -> Yuv444 {
    let (w, h) = (main.width, main.height);
    assert!(w % 2 == 0 && h % 2 == 0, "dimensions must be even");
    let mut out = Yuv444::new(w, h);
    out.y.copy_from_slice(&main.y);

    let half = h / 2;
    // even rows: U/V from main (even col) and aux (odd col).
    for by in 0..h / 2 {
        for bx in 0..w / 2 {
            let sy = 2 * by;
            let m = by * w + bx * 2;
            out.u[sy * w + 2 * bx] = main.uv[m];
            out.v[sy * w + 2 * bx] = main.uv[m + 1];
            out.u[sy * w + 2 * bx + 1] = aux.uv[m];
            out.v[sy * w + 2 * bx + 1] = aux.uv[m + 1];
        }
    }
    // odd rows: full-width U from aux top half, V from aux bottom half.
    for y in 0..half {
        let dst = (2 * y + 1) * w;
        out.u[dst..dst + w].copy_from_slice(&aux.y[y * w..y * w + w]);
        out.v[dst..dst + w].copy_from_slice(&aux.y[(half + y) * w..(half + y) * w + w]);
    }
    out
}

/// Upsample one NV12 (4:2:0) frame to 4:4:4 by duplicating each chroma sample
/// across its 2x2 block. Used for the low-bandwidth single-stream path.
pub fn nv12_to_yuv444(main: &Nv12) -> Yuv444 {
    let (w, h) = (main.width, main.height);
    let mut out = Yuv444::new(w, h);
    out.y.copy_from_slice(&main.y);
    for by in 0..h / 2 {
        for bx in 0..w / 2 {
            let u = main.uv[by * w + bx * 2];
            let v = main.uv[by * w + bx * 2 + 1];
            for dy in 0..2 {
                for dx in 0..2 {
                    let idx = (2 * by + dy) * w + 2 * bx + dx;
                    out.u[idx] = u;
                    out.v[idx] = v;
                }
            }
        }
    }
    out
}

/// Subsample 4:4:4 to a single NV12 (4:2:0) frame: the main frame of the split.
pub fn yuv444_to_nv12(src: &Yuv444) -> Nv12 {
    split_yuv444(src).0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filled(w: usize, h: usize) -> Yuv444 {
        let mut f = Yuv444::new(w, h);
        for i in 0..w * h {
            f.y[i] = (i * 7) as u8;
            f.u[i] = (i * 13 + 1) as u8;
            f.v[i] = (i * 29 + 2) as u8;
        }
        f
    }

    #[test]
    fn split_recombine_is_lossless() {
        for (w, h) in [(2, 2), (4, 4), (16, 16), (64, 32), (640, 360), (1920, 1080)] {
            let src = filled(w, h);
            let (main, aux) = split_yuv444(&src);
            assert_eq!(main.y.len(), w * h);
            assert_eq!(aux.uv.len(), w * h / 2);
            let back = recombine_yuv444(&main, &aux);
            assert_eq!(src, back, "lossless at {w}x{h}");
        }
    }

    #[test]
    fn sample_counts_match_444() {
        let (w, h) = (640, 360);
        let (main, aux) = split_yuv444(&filled(w, h));
        // main carries w*h/2 chroma samples (UV plane), aux carries w*h (its Y)
        // plus w*h/2 (its UV) = together 2*w*h chroma samples of 4:4:4.
        let chroma = main.uv.len() + aux.y.len() + aux.uv.len();
        assert_eq!(chroma, 2 * w * h);
    }
}
