//! BGRA <-> planar YUV 4:4:4, BT.709 limited range, on the CPU with rayon.
//!
//! This is the `CpuSplitter`'s colour stage: the one place in the pipeline that
//! touches system memory per frame. `GlSplitter` (phase 5) replaces it.

use rayon::prelude::*;

use haver_proto::chroma::Yuv444;

/// Convert BGRA (or BGRX) pixels with `stride` bytes per row to planar YUV 4:4:4.
pub fn bgra_to_yuv444(bgra: &[u8], stride: usize, width: usize, height: usize) -> Yuv444 {
    let mut out = Yuv444::new(width, height);
    let rows: Vec<(&mut [u8], &mut [u8], &mut [u8])> = {
        let y_rows = out.y.chunks_mut(width);
        let u_rows = out.u.chunks_mut(width);
        let v_rows = out.v.chunks_mut(width);
        y_rows.zip(u_rows).zip(v_rows).map(|((y, u), v)| (y, u, v)).collect()
    };
    rows.into_par_iter().enumerate().for_each(|(row, (y, u, v))| {
        let line = &bgra[row * stride..row * stride + width * 4];
        for col in 0..width {
            let px = &line[col * 4..col * 4 + 4];
            let (b, g, r) = (px[0] as f32, px[1] as f32, px[2] as f32);
            // BT.709 limited range.
            let yy = 16.0 + 0.1826 * r + 0.6142 * g + 0.0620 * b;
            let cb = 128.0 - 0.1006 * r - 0.3386 * g + 0.4392 * b;
            let cr = 128.0 + 0.4392 * r - 0.3989 * g - 0.0403 * b;
            y[col] = yy.round().clamp(0.0, 255.0) as u8;
            u[col] = cb.round().clamp(0.0, 255.0) as u8;
            v[col] = cr.round().clamp(0.0, 255.0) as u8;
        }
    });
    out
}

/// Convert planar YUV 4:4:4 (BT.709 limited range) back to BGRA.
pub fn yuv444_to_bgra(src: &Yuv444) -> Vec<u8> {
    let (w, h) = (src.width, src.height);
    let mut out = vec![0u8; w * h * 4];
    out.par_chunks_mut(w * 4).enumerate().for_each(|(row, line)| {
        for col in 0..w {
            let i = row * w + col;
            let yy = src.y[i] as f32 - 16.0;
            let cb = src.u[i] as f32 - 128.0;
            let cr = src.v[i] as f32 - 128.0;
            let r = 1.1644 * yy + 1.7927 * cr;
            let g = 1.1644 * yy - 0.2132 * cb - 0.5329 * cr;
            let b = 1.1644 * yy + 2.1124 * cb;
            let px = &mut line[col * 4..col * 4 + 4];
            px[0] = b.round().clamp(0.0, 255.0) as u8;
            px[1] = g.round().clamp(0.0, 255.0) as u8;
            px[2] = r.round().clamp(0.0, 255.0) as u8;
            px[3] = 255;
        }
    });
    out
}

/// Peak signal-to-noise ratio between two equal-length byte buffers.
pub fn psnr(a: &[u8], b: &[u8]) -> f64 {
    let n = a.len().min(b.len());
    if n == 0 {
        return 0.0;
    }
    let mse: f64 = a[..n].iter().zip(&b[..n]).map(|(x, y)| (*x as f64 - *y as f64).powi(2)).sum::<f64>() / n as f64;
    if mse == 0.0 {
        99.0
    } else {
        10.0 * (255.0f64 * 255.0 / mse).log10()
    }
}
