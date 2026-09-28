//! A paintable that shows a frame at a whole number of device pixels per
//! stream pixel.
//!
//! The server declares a full-quality `view` size in device pixels. When
//! the widget has room, the frame is drawn at the largest integer factor
//! that fits (1x, 2x, ...) and centred. The decoder is asked to write its
//! output at that same factor (pixel replication in the recombine shader,
//! or on the CPU), so the texture GTK receives is already device sized and
//! is drawn 1:1 with a plain texture node, which stays pixel-exact. GTK's
//! own nearest-neighbour scaling is not used: on a HiDPI surface its
//! renderer draws a scaled texture through an offscreen at logical
//! resolution, which blurs the result. When the widget is smaller than the
//! view, the frame shrinks to fit with linear filtering. A stream sent at
//! reduced resolution (view larger than the texture) is stretched to the
//! view box either way, so a quality change never moves the picture.
//!
//! [`layout`] is the single source of the frame's rectangle; the pointer
//! mapping and the zoom request in `main.rs` derive from it.

use std::cell::{Cell, RefCell};

use gtk::gdk;
use gtk::glib;
use gtk::graphene;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk4 as gtk;

/// Where the frame is drawn inside the widget, in logical pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Layout {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    /// Device pixels per view pixel; a whole number when the view fits.
    pub factor: f64,
}

impl Layout {
    /// The integer zoom to ask of the decoder: the factor when the view
    /// fits, else 1 (a shrunk frame is resampled anyway).
    pub fn zoom(&self) -> u32 {
        if self.factor >= 1.0 {
            self.factor as u32
        } else {
            1
        }
    }
}

/// Fit a `view` (device pixels) into a widget of `width` x `height` logical
/// pixels on a display of `device` scale: the largest integer factor that
/// fits, else a fractional shrink. Offsets are snapped to device pixels so
/// an integer factor stays pixel-aligned.
pub fn layout(view: (u32, u32), device: i32, width: f64, height: f64) -> Option<Layout> {
    let (vw, vh) = (view.0 as f64, view.1 as f64);
    if vw <= 0.0 || vh <= 0.0 || width <= 0.0 || height <= 0.0 {
        return None;
    }
    let device = device.max(1) as f64;
    let fit = (width * device / vw).min(height * device / vh);
    let factor = if fit >= 1.0 { fit.floor() } else { fit };
    let (fw, fh) = (vw * factor / device, vh * factor / device);
    let snap = |v: f64| (v * device).floor() / device;
    Some(Layout {
        x: snap((width - fw) / 2.0),
        y: snap((height - fh) / 2.0),
        width: fw,
        height: fh,
        factor,
    })
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct FramePaintable {
        pub texture: RefCell<Option<gdk::Texture>>,
        pub scale: Cell<i32>,
        /// The full-quality fit size in device pixels; the texture is
        /// stretched to it when the stream is scaled down.
        pub view: Cell<(u32, u32)>,
    }

    impl FramePaintable {
        fn view_or_texture(&self) -> (i32, i32) {
            let (vw, vh) = self.view.get();
            if vw > 0 && vh > 0 {
                return (vw as i32, vh as i32);
            }
            self.texture
                .borrow()
                .as_ref()
                .map_or((0, 0), |t| (t.width(), t.height()))
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for FramePaintable {
        const NAME: &'static str = "GliffFramePaintable";
        type Type = super::FramePaintable;
        type Interfaces = (gdk::Paintable,);
    }

    impl ObjectImpl for FramePaintable {}

    impl PaintableImpl for FramePaintable {
        fn intrinsic_width(&self) -> i32 {
            self.view_or_texture().0 / self.scale.get().max(1)
        }

        fn intrinsic_height(&self) -> i32 {
            self.view_or_texture().1 / self.scale.get().max(1)
        }

        fn intrinsic_aspect_ratio(&self) -> f64 {
            let (w, h) = self.view_or_texture();
            if h == 0 {
                return 0.0;
            }
            w as f64 / h as f64
        }

        fn snapshot(&self, snapshot: &gdk::Snapshot, width: f64, height: f64) {
            let Some(t) = self.texture.borrow().clone() else {
                return;
            };
            let (vw, vh) = self.view_or_texture();
            let Some(l) = layout((vw as u32, vh as u32), self.scale.get(), width, height) else {
                return;
            };
            let bounds =
                graphene::Rect::new(l.x as f32, l.y as f32, l.width as f32, l.height as f32);
            snapshot.append_texture(&t, &bounds);
        }
    }
}

glib::wrapper! {
    pub struct FramePaintable(ObjectSubclass<imp::FramePaintable>) @implements gdk::Paintable;
}

impl Default for FramePaintable {
    fn default() -> Self {
        let p: Self = glib::Object::new();
        p.imp().scale.set(1);
        p
    }
}

impl FramePaintable {
    /// Show `texture` inside a `view`-sized area (device pixels) on a
    /// display of `scale_factor`.
    pub fn set_frame(&self, texture: gdk::Texture, scale_factor: i32, view: (u32, u32)) {
        let imp = self.imp();
        let size_changed = imp.scale.get() != scale_factor || imp.view.get() != view;
        imp.scale.set(scale_factor);
        imp.view.set(view);
        *imp.texture.borrow_mut() = Some(texture);
        if size_changed {
            self.invalidate_size();
        }
        self.invalidate_contents();
    }

    /// Show nothing, as when no machine is connected.
    pub fn clear(&self) {
        let imp = self.imp();
        imp.view.set((0, 0));
        if imp.texture.borrow_mut().take().is_some() {
            self.invalidate_size();
            self.invalidate_contents();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doubles_when_the_widget_has_room() {
        let l = layout((2560, 1440), 2, 2560.0, 1440.0).unwrap();
        assert_eq!(l.factor, 2.0);
        assert_eq!((l.x, l.y, l.width, l.height), (0.0, 0.0, 2560.0, 1440.0));
    }

    #[test]
    fn drops_to_one_when_two_does_not_fit() {
        let l = layout((2560, 1440), 2, 2560.0, 1400.0).unwrap();
        assert_eq!(l.factor, 1.0);
        assert_eq!((l.width, l.height), (1280.0, 720.0));
        assert_eq!((l.x, l.y), (640.0, 340.0));
    }

    #[test]
    fn shrinks_fractionally_when_too_small() {
        let l = layout((2560, 1440), 1, 1280.0, 1000.0).unwrap();
        assert_eq!(l.factor, 0.5);
        assert_eq!((l.width, l.height), (1280.0, 720.0));
        assert_eq!((l.x, l.y), (0.0, 140.0));
    }

    #[test]
    fn offsets_snap_to_device_pixels() {
        let l = layout((1001, 601), 2, 1000.0, 600.0).unwrap();
        assert_eq!(l.factor, 1.0);
        assert_eq!(l.x * 2.0, (l.x * 2.0).floor());
        assert_eq!(l.y * 2.0, (l.y * 2.0).floor());
    }

    #[test]
    fn zoom_is_the_integer_factor_or_one() {
        assert_eq!(layout((2560, 1440), 2, 2560.0, 1440.0).unwrap().zoom(), 2);
        assert_eq!(layout((2560, 1440), 2, 2560.0, 1400.0).unwrap().zoom(), 1);
        assert_eq!(layout((2560, 1440), 1, 1280.0, 1000.0).unwrap().zoom(), 1);
    }

    #[test]
    fn empty_inputs_have_no_layout() {
        assert!(layout((0, 0), 2, 100.0, 100.0).is_none());
        assert!(layout((10, 10), 2, 0.0, 100.0).is_none());
    }
}
