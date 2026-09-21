//! Geometry and cursor helpers every client front end needs.

/// Where a stream frame sits in a view, in the view's own units (logical
/// pixels, or points on macOS). The frame is drawn at one stream pixel per
/// device pixel, centred, and only ever shrunk to fit, never enlarged.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrameRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    /// Device pixels per stream pixel: the fit factor times the device scale.
    stream_to_view: f64,
    stream: (u32, u32),
}

impl FrameRect {
    /// Lay out a `stream_width` x `stream_height` frame in a view of
    /// `view_width` x `view_height` units on a display with `device_scale`
    /// device pixels per unit.
    pub fn new(
        stream_width: u32,
        stream_height: u32,
        device_scale: f64,
        view_width: f64,
        view_height: f64,
    ) -> Self {
        let device = device_scale.max(0.01);
        let (lw, lh) = (stream_width as f64 / device, stream_height as f64 / device);
        let fit = if lw > 0.0 && lh > 0.0 {
            (view_width / lw).min(view_height / lh).min(1.0)
        } else {
            1.0
        };
        let (width, height) = (lw * fit, lh * fit);
        Self {
            x: (view_width - width) / 2.0,
            y: (view_height - height) / 2.0,
            width,
            height,
            stream_to_view: fit / device,
            stream: (stream_width, stream_height),
        }
    }

    /// Map a view point to the remote output's logical coordinates: undo the
    /// letterbox to physical stream pixels, then divide by the remote scale,
    /// which is what the server's virtual pointer expects.
    pub fn to_remote(&self, x: f64, y: f64, remote_scale: f64) -> (f64, f64) {
        let (rw, rh) = self.stream;
        if rw == 0 || rh == 0 || self.stream_to_view <= 0.0 {
            return (0.0, 0.0);
        }
        let px = ((x - self.x) / self.stream_to_view).clamp(0.0, rw as f64);
        let py = ((y - self.y) / self.stream_to_view).clamp(0.0, rh as f64);
        let scale = remote_scale.max(0.01);
        (px / scale, py / scale)
    }
}

/// True if a remote cursor image has a visible shape. Hyprland sends fully
/// transparent images (and flat black ones for other outputs) when it has no
/// cursor image to share; those should fall back to the local default.
pub fn has_visible_shape(argb: &[u8]) -> bool {
    let mut pixels = argb.chunks_exact(4);
    let Some(first) = pixels.next() else {
        return false;
    };
    let opaque = first[3] != 0 || pixels.clone().any(|p| p[3] != 0);
    let uniform = pixels.all(|p| p == first);
    opaque && !uniform
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_shape_needs_alpha_and_contrast() {
        let transparent = [0u8; 16];
        let black = [0, 0, 0, 255].repeat(4);
        let mut arrow = [0u8; 16];
        arrow[3] = 255;
        assert!(!has_visible_shape(&transparent));
        assert!(!has_visible_shape(&black));
        assert!(has_visible_shape(&arrow));
    }

    #[test]
    fn frame_fits_one_to_one_and_centres() {
        // A 2000x1000 stream on a 2x display is 1000x500 units; in a
        // 1200x700 view it is shown 1:1, centred.
        let r = FrameRect::new(2000, 1000, 2.0, 1200.0, 700.0);
        assert_eq!((r.x, r.y, r.width, r.height), (100.0, 100.0, 1000.0, 500.0));
        // The view's centre is the stream's centre; remote scale 2 halves it.
        assert_eq!(r.to_remote(600.0, 350.0, 2.0), (500.0, 250.0));
        assert_eq!(r.to_remote(600.0, 350.0, 1.0), (1000.0, 500.0));
    }

    #[test]
    fn frame_shrinks_but_never_grows() {
        let small = FrameRect::new(2000, 1000, 1.0, 1000.0, 1000.0);
        assert_eq!((small.width, small.height), (1000.0, 500.0));
        assert_eq!(small.to_remote(1000.0, 750.0, 1.0), (2000.0, 1000.0));
        let big = FrameRect::new(200, 100, 1.0, 1000.0, 1000.0);
        assert_eq!((big.width, big.height), (200.0, 100.0));
    }

    #[test]
    fn points_outside_the_frame_clamp_to_its_edge() {
        let r = FrameRect::new(2000, 1000, 2.0, 1200.0, 700.0);
        assert_eq!(r.to_remote(0.0, 0.0, 1.0), (0.0, 0.0));
        assert_eq!(r.to_remote(5000.0, 5000.0, 1.0), (2000.0, 1000.0));
    }
}
