//! Headless (surfaceless) GL context on a DRM render node, for the server-side
//! GPU split: render into the encoder's VA surfaces (imported as dmabufs) with
//! no window. All `unsafe` (EGL/GL C APIs) is contained here and in `lib.rs`.

use std::ffi::{c_void, CString};
use std::path::Path;

use drm_fourcc::DrmFourcc;
use glow::HasContext;
use khronos_egl as egl;

use crate::{DmabufPlane, Error, ImageTargetTexture2D, Result};

const EGL_LINUX_DMABUF_EXT: egl::Enum = 0x3270;
const EGL_WIDTH: egl::Attrib = 0x3057;
const EGL_HEIGHT: egl::Attrib = 0x3056;
const EGL_LINUX_DRM_FOURCC_EXT: egl::Attrib = 0x3271;
const EGL_DMA_BUF_PLANE0_FD_EXT: egl::Attrib = 0x3272;
const EGL_DMA_BUF_PLANE0_OFFSET_EXT: egl::Attrib = 0x3273;
const EGL_DMA_BUF_PLANE0_PITCH_EXT: egl::Attrib = 0x3274;
const EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT: egl::Attrib = 0x3443;
const EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT: egl::Attrib = 0x3444;
const GL_TEXTURE_2D: u32 = 0x0DE1;
const DMABUF_IMPORT_EXT: &str = "EGL_EXT_image_dma_buf_import";

/// A surfaceless GL ES context bound to a render node.
pub struct Headless {
    egl: egl::Instance<egl::Static>,
    display: egl::Display,
    context: egl::Context,
    gl: glow::Context,
    image_target: ImageTargetTexture2D,
    has_modifiers: bool,
    // AVC444 split programs, one per output plane; None until first use.
    split: Option<SplitProgs>,
    // Keep the GBM device alive for the display's lifetime.
    _device: gbm::Device<std::fs::File>,
}

struct SplitProgs {
    main_y: glow::Program,
    main_uv: glow::Program,
    aux_y: glow::Program,
    aux_uv: glow::Program,
    vao: glow::VertexArray,
    src_tex: glow::NativeTexture,
}

impl Headless {
    pub fn new(render_node: &Path) -> Result<Self> {
        let file = std::fs::File::options()
            .read(true)
            .write(true)
            .open(render_node)
            .map_err(|e| Error::Egl(format!("open {}: {e}", render_node.display())))?;
        let device = gbm::Device::new(file).map_err(|e| Error::Egl(format!("gbm: {e}")))?;
        let egl = egl::Instance::new(egl::Static);
        use gbm::AsRaw;
        // SAFETY: the gbm device pointer is a valid EGL native display on Mesa.
        let display =
            unsafe { egl.get_display(device.as_raw() as *mut c_void) }.ok_or(Error::NoDisplay)?;
        egl.initialize(display)
            .map_err(|e| Error::Egl(e.to_string()))?;
        let exts = egl
            .query_string(Some(display), egl::EXTENSIONS)
            .map_err(|e| Error::Egl(e.to_string()))?;
        if !exts.to_string_lossy().contains(DMABUF_IMPORT_EXT) {
            return Err(Error::NoDmabufImport);
        }
        egl.bind_api(egl::OPENGL_ES_API)
            .map_err(|e| Error::Egl(e.to_string()))?;
        let has_modifiers = exts
            .to_string_lossy()
            .contains("EGL_EXT_image_dma_buf_import_modifiers");
        // Surfaceless rendering only uses FBOs, so a config is unnecessary:
        // EGL_KHR_no_config_context lets us create a context with no config.
        // SAFETY: EGL_NO_CONFIG_KHR is the documented null config sentinel.
        let no_config = unsafe { egl::Config::from_ptr(std::ptr::null_mut()) };
        let context = egl
            .create_context(
                display,
                no_config,
                None,
                &[egl::CONTEXT_MAJOR_VERSION, 3, egl::NONE],
            )
            .map_err(|e| Error::Egl(format!("create context: {e}")))?;
        // Surfaceless: requires EGL_KHR_surfaceless_context (standard on Mesa).
        egl.make_current(display, None, None, Some(context))
            .map_err(|e| Error::Egl(format!("make current: {e}")))?;

        let name = CString::new("glEGLImageTargetTexture2DOES").unwrap();
        let raw = egl
            .get_proc_address(name.to_str().unwrap())
            .ok_or_else(|| Error::Gl("glEGLImageTargetTexture2DOES missing".into()))?;
        // SAFETY: signature (GLenum target, GLeglImageOES image).
        let image_target: ImageTargetTexture2D =
            unsafe { std::mem::transmute::<extern "system" fn(), ImageTargetTexture2D>(raw) };
        // SAFETY: get_proc_address gives valid GL entry points for this context.
        let gl = unsafe {
            glow::Context::from_loader_function(|s| match egl.get_proc_address(s) {
                Some(f) => f as *const c_void,
                None => std::ptr::null(),
            })
        };
        Ok(Self {
            egl,
            display,
            context,
            gl,
            image_target,
            has_modifiers,
            split: None,
            _device: device,
        })
    }

    fn ensure_split(&mut self) -> Result<()> {
        if self.split.is_some() {
            return Ok(());
        }
        // SAFETY: compile the split programs and a vao on the current context.
        let progs = unsafe {
            SplitProgs {
                main_y: crate::build_program(
                    &self.gl,
                    crate::VERT,
                    &format!("{HEAD}{FRAG_MAIN_Y}"),
                )?,
                main_uv: crate::build_program(
                    &self.gl,
                    crate::VERT,
                    &format!("{HEAD}{FRAG_MAIN_UV}"),
                )?,
                aux_y: crate::build_program(&self.gl, crate::VERT, &format!("{HEAD}{FRAG_AUX_Y}"))?,
                aux_uv: crate::build_program(
                    &self.gl,
                    crate::VERT,
                    &format!("{HEAD}{FRAG_AUX_UV}"),
                )?,
                vao: self.gl.create_vertex_array().map_err(Error::Gl)?,
                src_tex: crate::new_texture(&self.gl)?,
            }
        };
        self.split = Some(progs);
        Ok(())
    }

    /// Render the AVC444 split of a captured BGRA frame into the four NV12 dmabuf
    /// planes of the encoder's main and auxiliary surfaces, entirely on the GPU.
    /// `width`/`height` are the display size; the plane textures may be larger
    /// (coded, 16-aligned).
    #[allow(clippy::too_many_arguments)]
    pub fn split_dual(
        &mut self,
        input: &DmabufPlane,
        width: u32,
        height: u32,
        main_y: &DmabufPlane,
        main_uv: &DmabufPlane,
        aux_y: &DmabufPlane,
        aux_uv: &DmabufPlane,
    ) -> Result<()> {
        self.make_current()?;
        self.ensure_split()?;
        let src_image = self.import(input)?;
        let mut images = vec![src_image];
        let result = (|| -> Result<()> {
            let progs = self.split.as_ref().expect("ensured");
            // SAFETY: bind the BGRA input to the shared source texture.
            unsafe {
                self.gl.active_texture(glow::TEXTURE0);
                self.gl.bind_texture(GL_TEXTURE_2D, Some(progs.src_tex));
                (self.image_target)(GL_TEXTURE_2D, images[0].as_ptr());
                self.gl.bind_vertex_array(Some(progs.vao));
            }
            // Render each output plane into its dmabuf.
            let mut pass =
                |prog: glow::Program, plane: &DmabufPlane, pw: u32, ph: u32| -> Result<()> {
                    let image = self.import(plane)?;
                    images.push(image);
                    // SAFETY: attach the plane image to an FBO and draw the pass.
                    unsafe {
                        let tex = crate::new_texture(&self.gl)?;
                        self.gl.bind_texture(GL_TEXTURE_2D, Some(tex));
                        (self.image_target)(GL_TEXTURE_2D, images.last().unwrap().as_ptr());
                        let fbo = self.gl.create_framebuffer().map_err(Error::Gl)?;
                        self.gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
                        self.gl.framebuffer_texture_2d(
                            glow::FRAMEBUFFER,
                            glow::COLOR_ATTACHMENT0,
                            GL_TEXTURE_2D,
                            Some(tex),
                            0,
                        );
                        let status = self.gl.check_framebuffer_status(glow::FRAMEBUFFER);
                        if status != glow::FRAMEBUFFER_COMPLETE {
                            self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
                            self.gl.delete_framebuffer(fbo);
                            self.gl.delete_texture(tex);
                            return Err(Error::Gl(format!("split fbo incomplete: {status:#x}")));
                        }
                        self.gl.viewport(0, 0, pw as i32, ph as i32);
                        self.gl.use_program(Some(prog));
                        self.gl.active_texture(glow::TEXTURE0);
                        self.gl.bind_texture(GL_TEXTURE_2D, Some(progs.src_tex));
                        if let Some(l) = self.gl.get_uniform_location(prog, "src") {
                            self.gl.uniform_1_i32(Some(&l), 0);
                        }
                        if let Some(l) = self.gl.get_uniform_location(prog, "tex_w") {
                            self.gl.uniform_1_i32(Some(&l), width as i32);
                        }
                        if let Some(l) = self.gl.get_uniform_location(prog, "tex_h") {
                            self.gl.uniform_1_i32(Some(&l), height as i32);
                        }
                        if let Some(l) = self.gl.get_uniform_location(prog, "out_w") {
                            self.gl.uniform_1_i32(Some(&l), pw as i32);
                        }
                        if let Some(l) = self.gl.get_uniform_location(prog, "out_h") {
                            self.gl.uniform_1_i32(Some(&l), ph as i32);
                        }
                        self.gl.draw_arrays(glow::TRIANGLES, 0, 3);
                        self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
                        self.gl.delete_framebuffer(fbo);
                        self.gl.delete_texture(tex);
                    }
                    Ok(())
                };
            pass(progs.main_y, main_y, width, height)?;
            pass(progs.main_uv, main_uv, width / 2, height / 2)?;
            pass(progs.aux_y, aux_y, width, height)?;
            pass(progs.aux_uv, aux_uv, width / 2, height / 2)?;
            // SAFETY: block until the GPU finished writing the dmabufs, so the
            // encoder reads completed pixels.
            unsafe {
                self.gl.bind_vertex_array(None);
                self.gl.finish();
            }
            Ok(())
        })();
        for image in images {
            let _ = self.egl.destroy_image(self.display, image);
        }
        result
    }

    /// Make this context current (call before any GL work on this thread).
    pub fn make_current(&self) -> Result<()> {
        self.egl
            .make_current(self.display, None, None, Some(self.context))
            .map_err(|e| Error::Egl(e.to_string()))
    }

    fn import(&self, p: &DmabufPlane) -> Result<egl::Image> {
        use std::os::fd::AsRawFd;
        #[rustfmt::skip]
        let mut attribs: Vec<egl::Attrib> = vec![
            EGL_WIDTH, p.width as egl::Attrib,
            EGL_HEIGHT, p.height as egl::Attrib,
            EGL_LINUX_DRM_FOURCC_EXT, p.fourcc as u32 as egl::Attrib,
            EGL_DMA_BUF_PLANE0_FD_EXT, p.fd.as_raw_fd() as egl::Attrib,
            EGL_DMA_BUF_PLANE0_OFFSET_EXT, p.offset as egl::Attrib,
            EGL_DMA_BUF_PLANE0_PITCH_EXT, p.stride as egl::Attrib,
        ];
        // VA surfaces are tiled; pass the modifier (needs the modifiers ext).
        let invalid = p.modifier == 0x00ff_ffff_ffff_ffff;
        if self.has_modifiers && p.modifier != 0 && !invalid {
            attribs.push(EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT);
            attribs.push((p.modifier & 0xffff_ffff) as egl::Attrib);
            attribs.push(EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT);
            attribs.push((p.modifier >> 32) as egl::Attrib);
        }
        attribs.push(egl::ATTRIB_NONE);
        // SAFETY: standard EGL_LINUX_DMABUF_EXT import args; NONE-terminated.
        let (ctx, buffer) = unsafe {
            (
                egl::Context::from_ptr(egl::NO_CONTEXT),
                egl::ClientBuffer::from_ptr(std::ptr::null_mut()),
            )
        };
        self.egl
            .create_image(self.display, ctx, EGL_LINUX_DMABUF_EXT, buffer, &attribs)
            .map_err(|e| Error::Egl(format!("import ({:?}): {e}", p.fourcc)))
    }

    /// Feasibility check: import `plane` as a render target, clear it to a known
    /// value, and confirm the framebuffer is complete. Returns the value written
    /// to the red channel (so the caller can read the surface back and compare).
    pub fn clear_plane(&self, plane: &DmabufPlane, red: f32) -> Result<()> {
        self.make_current()?;
        let image = self.import(plane)?;
        // SAFETY: bind the imported image to a texture, attach to an FBO, clear.
        let result = unsafe {
            let tex = self.gl.create_texture().map_err(Error::Gl)?;
            self.gl.bind_texture(GL_TEXTURE_2D, Some(tex));
            (self.image_target)(GL_TEXTURE_2D, image.as_ptr());
            let fbo = self.gl.create_framebuffer().map_err(Error::Gl)?;
            self.gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
            self.gl.framebuffer_texture_2d(
                glow::FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                GL_TEXTURE_2D,
                Some(tex),
                0,
            );
            let status = self.gl.check_framebuffer_status(glow::FRAMEBUFFER);
            let r = if status == glow::FRAMEBUFFER_COMPLETE {
                self.gl
                    .viewport(0, 0, plane.width as i32, plane.height as i32);
                self.gl.clear_color(red, 0.0, 0.0, 1.0);
                self.gl.clear(glow::COLOR_BUFFER_BIT);
                self.gl.finish();
                Ok(())
            } else {
                Err(Error::Gl(format!("framebuffer incomplete: {status:#x}")))
            };
            self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            self.gl.delete_framebuffer(fbo);
            self.gl.delete_texture(tex);
            r
        };
        let _ = self.egl.destroy_image(self.display, image);
        result
    }

    /// True if a fourcc is one we render NV12 planes with.
    pub fn renderable(fourcc: DrmFourcc) -> bool {
        matches!(fourcc, DrmFourcc::R8 | DrmFourcc::Gr88)
    }
}

// The split shaders read the BGRA source and write one NV12 plane each, using
// gl_FragCoord directly (texel row 0 == image top for both the imported source
// dmabuf and the FBO-attached destination, so no vertical flip is needed). BT.709
// limited-range coefficients match haver-codec::color::bgra_to_yuv444.
const HEAD: &str = r#"#version 300 es
precision highp float;
precision highp int;
out vec4 frag;
uniform sampler2D src;
uniform int tex_w;
uniform int tex_h;
uniform int out_w;
uniform int out_h;
vec3 rgb_at(int x, int y) {
    x = clamp(x, 0, tex_w - 1);
    y = clamp(y, 0, tex_h - 1);
    return texelFetch(src, ivec2(x, y), 0).rgb * 255.0;
}
float yy(vec3 c) { return (16.0 + 0.1826 * c.r + 0.6142 * c.g + 0.0620 * c.b) / 255.0; }
float uu(vec3 c) { return (128.0 - 0.1006 * c.r - 0.3386 * c.g + 0.4392 * c.b) / 255.0; }
float vv(vec3 c) { return (128.0 + 0.4392 * c.r - 0.3989 * c.g - 0.0403 * c.b) / 255.0; }
"#;

const FRAG_MAIN_Y: &str = r#"
void main() {
    int px = int(gl_FragCoord.x);
    int py = int(gl_FragCoord.y);
    frag = vec4(yy(rgb_at(px, py)), 0.0, 0.0, 1.0);
}
"#;

const FRAG_MAIN_UV: &str = r#"
void main() {
    int px = int(gl_FragCoord.x);
    int py = int(gl_FragCoord.y);
    vec3 c = rgb_at(2 * px, 2 * py);
    frag = vec4(uu(c), vv(c), 0.0, 1.0);
}
"#;

const FRAG_AUX_Y: &str = r#"
void main() {
    int px = int(gl_FragCoord.x);
    int py = int(gl_FragCoord.y);
    int halfh = out_h / 2;
    if (py < halfh) {
        frag = vec4(uu(rgb_at(px, 2 * py + 1)), 0.0, 0.0, 1.0);
    } else {
        int y2 = py - halfh;
        frag = vec4(vv(rgb_at(px, 2 * y2 + 1)), 0.0, 0.0, 1.0);
    }
}
"#;

const FRAG_AUX_UV: &str = r#"
void main() {
    int px = int(gl_FragCoord.x);
    int py = int(gl_FragCoord.y);
    vec3 c = rgb_at(2 * px + 1, 2 * py);
    frag = vec4(uu(c), vv(c), 0.0, 1.0);
}
"#;

impl Drop for Headless {
    fn drop(&mut self) {
        let _ = self.egl.make_current(self.display, None, None, None);
        let _ = self.egl.destroy_context(self.display, self.context);
        let _ = self.egl.terminate(self.display);
    }
}
