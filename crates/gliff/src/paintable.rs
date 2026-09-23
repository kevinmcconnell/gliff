//! A paintable that shows a frame at the stream's full-quality view size.
//!
//! GTK measures a texture in logical pixels, so on a HiDPI display a 1:1
//! stream would be drawn `scale_factor` times too large. This paintable
//! reports its size as `view / scale_factor`, where `view` is the
//! full-quality fit size the server declared; with `ContentFit::ScaleDown`
//! the picture then shows the frame 1:1 when it fits and shrinks it when
//! the window is smaller, but never enlarges it beyond the view. A stream
//! sent at reduced resolution (view larger than the texture) is stretched
//! up to the view size, so it fills the same area on screen instead of
//! drawing small.

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
