//! A paintable that shows a frame at one stream pixel per device pixel.
//!
//! GTK measures a texture in logical pixels, so on a HiDPI display a 1:1
//! stream would be drawn `scale_factor` times too large. This paintable
//! reports its size as `texture / scale_factor`; with `ContentFit::ScaleDown`
//! the picture then shows the frame at exactly 1:1 when it fits and shrinks
//! it when the window is smaller, but never enlarges it.

use std::cell::{Cell, RefCell};

use gtk::gdk;
use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk4 as gtk;

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct FramePaintable {
        pub texture: RefCell<Option<gdk::Texture>>,
        pub scale: Cell<i32>,
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
            self.texture
                .borrow()
                .as_ref()
                .map_or(0, |t| t.width() / self.scale.get().max(1))
        }

        fn intrinsic_height(&self) -> i32 {
            self.texture
                .borrow()
                .as_ref()
                .map_or(0, |t| t.height() / self.scale.get().max(1))
        }

        fn intrinsic_aspect_ratio(&self) -> f64 {
            self.texture
                .borrow()
                .as_ref()
                .map_or(0.0, |t| t.width() as f64 / t.height().max(1) as f64)
        }

        fn snapshot(&self, snapshot: &gdk::Snapshot, width: f64, height: f64) {
            if let Some(t) = self.texture.borrow().as_ref() {
                t.snapshot(snapshot, width, height);
            }
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
    /// Show `texture`, drawn 1:1 on a display of `scale_factor`.
    pub fn set_frame(&self, texture: gdk::Texture, scale_factor: i32) {
        let imp = self.imp();
        let size_changed = imp.scale.get() != scale_factor
            || imp
                .texture
                .borrow()
                .as_ref()
                .is_none_or(|t| (t.width(), t.height()) != (texture.width(), texture.height()));
        imp.scale.set(scale_factor);
        *imp.texture.borrow_mut() = Some(texture);
        if size_changed {
            self.invalidate_size();
        }
        self.invalidate_contents();
    }
}
