//! Encoder settings shared by every codec backend.

#[derive(Debug, Clone)]
pub struct EncoderSettings {
    pub width: u32,
    pub height: u32,
    /// Target bitrate in bits per second (CBR).
    pub bitrate: u32,
    pub framerate: u32,
    /// Rate control buffer: how far one frame may overshoot the average.
    /// Small on a slow link so a keyframe cannot stall it.
    pub vbv_ms: u32,
}

impl EncoderSettings {
    pub const DEFAULT_VBV_MS: u32 = 500;

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

    /// The largest even size at most `width`x`height` with the same aspect
    /// ratio whose 16-aligned coded size fits inside `max`.
    pub fn fit_extent(width: u32, height: u32, max: (u32, u32)) -> (u32, u32) {
        let (max_w, max_h) = (max.0 & !15, max.1 & !15);
        if width <= max_w && height <= max_h {
            return (width, height);
        }
        let scale = (max_w as f64 / width.max(1) as f64).min(max_h as f64 / height.max(1) as f64);
        let fit = |v: u32| (((v as f64 * scale).floor() as u32) & !1).max(2);
        (fit(width), fit(height))
    }
}

#[cfg(test)]
mod tests {
    use super::EncoderSettings;

    fn coded(v: u32) -> u32 {
        v.div_ceil(16) * 16
    }

    #[test]
    fn fit_extent_keeps_a_size_that_fits() {
        assert_eq!(
            EncoderSettings::fit_extent(1920, 1080, (4096, 4096)),
            (1920, 1080)
        );
        assert_eq!(
            EncoderSettings::fit_extent(4096, 2304, (4096, 4096)),
            (4096, 2304)
        );
    }

    #[test]
    fn fit_extent_scales_5k_to_the_encoder_maximum() {
        assert_eq!(
            EncoderSettings::fit_extent(5120, 2880, (4096, 4096)),
            (4096, 2304)
        );
    }

    #[test]
    fn fit_extent_scales_a_tall_output_by_height() {
        assert_eq!(
            EncoderSettings::fit_extent(2880, 5120, (4096, 4096)),
            (2304, 4096)
        );
    }

    #[test]
    fn fit_extent_result_is_even_and_codes_within_the_maximum() {
        let max = (4096, 2304);
        for (w, h) in [
            (5120, 2880),
            (7680, 4320),
            (4097, 2305),
            (3000, 3000),
            (321, 9999),
        ] {
            let (fw, fh) = EncoderSettings::fit_extent(w, h, max);
            assert_eq!(fw % 2, 0, "{w}x{h}");
            assert_eq!(fh % 2, 0, "{w}x{h}");
            assert!(
                coded(fw) <= max.0 && coded(fh) <= max.1,
                "{w}x{h} -> {fw}x{fh}"
            );
        }
    }
}
