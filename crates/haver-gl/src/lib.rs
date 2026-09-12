//! GPU display path: import decoded NV12 dmabufs as textures and recombine
//! AVC444 4:4:4 into RGB in one shader pass. Falls back to uploading CPU BGRA
//! (a plain textured blit) when dmabuf import is unavailable, so the window is
//! never left black.
//!
//! This crate is the ONLY place in haver that uses `unsafe`, because GL and EGL
//! are C APIs: `glow` marks every GL call `unsafe`, EGL image import needs raw
//! pointers, and `glEGLImageTargetTexture2DOES` is loaded at run time. Every
//! `unsafe` block has a `// SAFETY:` comment.

use std::ffi::{c_void, CString};
use std::os::fd::{AsRawFd, BorrowedFd};
use std::path::Path;

use drm_fourcc::DrmFourcc;
use glow::HasContext;
use khronos_egl as egl;

pub mod headless;
pub use headless::Headless;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("EGL: {0}")]
    Egl(String),
    #[error("GL: {0}")]
    Gl(String),
    #[error("dmabuf import is not available in this GL context")]
    NoDmabufImport,
    #[error("no current EGL display (call inside a current GL context)")]
    NoDisplay,
}

pub type Result<T> = std::result::Result<T, Error>;

const EGL_LINUX_DMABUF_EXT: egl::Enum = 0x3270;
const EGL_WIDTH: egl::Attrib = 0x3057;
const EGL_HEIGHT: egl::Attrib = 0x3056;
const EGL_LINUX_DRM_FOURCC_EXT: egl::Attrib = 0x3271;
const EGL_DMA_BUF_PLANE0_FD_EXT: egl::Attrib = 0x3272;
const EGL_DMA_BUF_PLANE0_OFFSET_EXT: egl::Attrib = 0x3273;
const EGL_DMA_BUF_PLANE0_PITCH_EXT: egl::Attrib = 0x3274;
const GL_TEXTURE_2D: u32 = 0x0DE1;
const DMABUF_IMPORT_EXT: &str = "EGL_EXT_image_dma_buf_import";

/// A single dmabuf plane to import as one texture.
pub struct DmabufPlane<'a> {
    pub fd: BorrowedFd<'a>,
    pub width: u32,
    pub height: u32,
    pub offset: u32,
    pub stride: u32,
    /// DRM fourcc, e.g. `DrmFourcc::R8` for luma, `Gr88` for NV12 UV.
    pub fourcc: DrmFourcc,
    pub modifier: u64,
}

/// The planes of an AVC444 frame: main NV12 and (for 4:4:4) auxiliary NV12.
pub struct FramePlanes<'a> {
    pub main_y: DmabufPlane<'a>,
    pub main_uv: DmabufPlane<'a>,
    pub aux: Option<(DmabufPlane<'a>, DmabufPlane<'a>)>,
    pub width: u32,
    pub height: u32,
}

/// True if EGL on `render_node` can import dmabufs. Used for the initial
/// display-path choice; the renderer re-checks the real context and falls back
/// at run time regardless, so a wrong answer here is not fatal.
pub fn supports_dmabuf_import(render_node: &Path) -> bool {
    probe(render_node).unwrap_or(false)
}

fn probe(render_node: &Path) -> Result<bool> {
    let file = std::fs::File::options()
        .read(true)
        .write(true)
        .open(render_node)
        .map_err(|e| Error::Egl(format!("open render node: {e}")))?;
    let device = gbm::Device::new(file).map_err(|e| Error::Egl(format!("gbm: {e}")))?;
    let egl = egl::Instance::new(egl::Static);
    use gbm::AsRaw;
    // SAFETY: the gbm device pointer is a valid EGL native display on Mesa; the
    // Display it yields is only used to initialize and query strings.
    let display = unsafe { egl.get_display(device.as_raw() as *mut c_void) }.ok_or(Error::NoDisplay)?;
    egl.initialize(display).map_err(|e| Error::Egl(e.to_string()))?;
    let exts = egl.query_string(Some(display), egl::EXTENSIONS).map_err(|e| Error::Egl(e.to_string()))?;
    let ok = exts.to_string_lossy().contains(DMABUF_IMPORT_EXT);
    let _ = egl.terminate(display);
    Ok(ok)
}

pub(crate) type ImageTargetTexture2D = extern "system" fn(u32, *mut c_void);

/// Recombines decoded NV12 dmabufs (or blits CPU BGRA) with one shader pass.
/// Create it inside a current GL context (a `gtk::GLArea` render callback).
pub struct Renderer {
    egl: egl::Instance<egl::Static>,
    gl: glow::Context,
    recombine: glow::Program,
    blit: glow::Program,
    vao: glow::VertexArray,
    plane_textures: [glow::NativeTexture; 4],
    rgba_texture: glow::NativeTexture,
    image_target: Option<ImageTargetTexture2D>,
    can_dmabuf: bool,
}

impl Renderer {
    /// Build the programs in the current GL context. Succeeds even without
    /// dmabuf import (then only `draw_rgba` works); check [`Renderer::can_dmabuf`].
    pub fn new() -> Result<Self> {
        let egl = egl::Instance::new(egl::Static);
        let has_ext = match egl.get_current_display() {
            Some(display) => egl
                .query_string(Some(display), egl::EXTENSIONS)
                .map(|e| e.to_string_lossy().contains(DMABUF_IMPORT_EXT))
                .unwrap_or(false),
            None => false,
        };
        let image_target = if has_ext {
            let name = CString::new("glEGLImageTargetTexture2DOES").unwrap();
            egl.get_proc_address(name.to_str().unwrap()).map(|raw| {
                // SAFETY: this EGL entry point has signature
                // (GLenum target, GLeglImageOES image); we type it as such.
                unsafe { std::mem::transmute::<extern "system" fn(), ImageTargetTexture2D>(raw) }
            })
        } else {
            None
        };
        let can_dmabuf = has_ext && image_target.is_some();

        // SAFETY: get_proc_address returns valid GL entry points for the current
        // context; glow only calls them while that context is current.
        let gl = unsafe {
            glow::Context::from_loader_function(|s| match egl.get_proc_address(s) {
                Some(f) => f as *const c_void,
                None => std::ptr::null(),
            })
        };

        // SAFETY: standard GL object creation on the current context.
        let (recombine, blit, vao, plane_textures, rgba_texture) = unsafe {
            let recombine = build_program(&gl, VERT, FRAG_RECOMBINE)?;
            let blit = build_program(&gl, VERT, FRAG_BLIT)?;
            let vao = gl.create_vertex_array().map_err(Error::Gl)?;
            let plane_textures = [new_texture(&gl)?, new_texture(&gl)?, new_texture(&gl)?, new_texture(&gl)?];
            let rgba_texture = new_texture(&gl)?;
            (recombine, blit, vao, plane_textures, rgba_texture)
        };
        Ok(Self { egl, gl, recombine, blit, vao, plane_textures, rgba_texture, image_target, can_dmabuf })
    }

    pub fn can_dmabuf(&self) -> bool {
        self.can_dmabuf
    }

    /// Draw one 4:4:4 frame from dmabuf planes. Errors (leaving the framebuffer
    /// untouched) if dmabuf import is unavailable or fails; the caller then
    /// switches to the CPU path.
    pub fn draw_planes(&self, planes: &FramePlanes, fb_w: i32, fb_h: i32) -> Result<()> {
        let image_target = self.image_target.filter(|_| self.can_dmabuf).ok_or(Error::NoDmabufImport)?;
        let display = self.egl.get_current_display().ok_or(Error::NoDisplay)?;
        let (vw, vh) = (planes.width as i32, planes.height as i32);

        let mut images: Vec<egl::Image> = Vec::new();
        let result = (|| -> Result<()> {
            let mut bind = |unit: usize, plane: &DmabufPlane| -> Result<()> {
                let image = self.import(display, plane)?;
                images.push(image);
                // SAFETY: unit < 4, texture handles valid; the OES entry point
                // binds the EGLImage to the bound 2D texture.
                unsafe {
                    self.gl.active_texture(glow::TEXTURE0 + unit as u32);
                    self.gl.bind_texture(GL_TEXTURE_2D, Some(self.plane_textures[unit]));
                    image_target(GL_TEXTURE_2D, image.as_ptr());
                }
                Ok(())
            };
            bind(0, &planes.main_y)?;
            bind(1, &planes.main_uv)?;
            if let Some((ay, auv)) = &planes.aux {
                bind(2, ay)?;
                bind(3, auv)?;
            }
            Ok(())
        })();
        if let Err(e) = result {
            for image in images {
                let _ = self.egl.destroy_image(display, image);
            }
            return Err(e);
        }

        // SAFETY: uniforms/enums valid for the program; single-triangle draw.
        unsafe {
            self.letterbox(fb_w, fb_h, vw, vh);
            self.gl.use_program(Some(self.recombine));
            self.set_i32(self.recombine, "main_y", 0);
            self.set_i32(self.recombine, "main_uv", 1);
            self.set_i32(self.recombine, "aux_y", 2);
            self.set_i32(self.recombine, "aux_uv", 3);
            self.set_i32(self.recombine, "has_aux", planes.aux.is_some() as i32);
            self.set_i32(self.recombine, "tex_w", vw);
            self.set_i32(self.recombine, "tex_h", vh);
            self.gl.bind_vertex_array(Some(self.vao));
            self.gl.draw_arrays(glow::TRIANGLES, 0, 3);
            self.gl.bind_vertex_array(None);
        }
        for image in images {
            let _ = self.egl.destroy_image(display, image);
        }
        Ok(())
    }

    /// Draw one frame from CPU BGRA (the fallback when dmabuf import is off).
    pub fn draw_rgba(&self, bgra: &[u8], width: i32, height: i32, fb_w: i32, fb_h: i32) -> Result<()> {
        if width <= 0 || height <= 0 || bgra.len() < (width * height * 4) as usize {
            return Err(Error::Gl("rgba buffer too small".into()));
        }
        // SAFETY: upload the client buffer to an RGBA texture and blit it; the
        // slice outlives the tex_image_2d call.
        unsafe {
            self.gl.active_texture(glow::TEXTURE0);
            self.gl.bind_texture(GL_TEXTURE_2D, Some(self.rgba_texture));
            self.gl.tex_image_2d(
                GL_TEXTURE_2D,
                0,
                glow::RGBA as i32,
                width,
                height,
                0,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(Some(bgra)),
            );
            self.letterbox(fb_w, fb_h, width, height);
            self.gl.use_program(Some(self.blit));
            self.set_i32(self.blit, "tex", 0);
            self.gl.bind_vertex_array(Some(self.vao));
            self.gl.draw_arrays(glow::TRIANGLES, 0, 3);
            self.gl.bind_vertex_array(None);
        }
        Ok(())
    }

    /// Clear the whole framebuffer and set a Contain-letterboxed viewport.
    ///
    /// # Safety
    /// A current GL context.
    unsafe fn letterbox(&self, fb_w: i32, fb_h: i32, vw: i32, vh: i32) {
        // SAFETY: viewport/clear on the current context.
        unsafe {
            self.gl.viewport(0, 0, fb_w, fb_h);
            self.gl.clear_color(0.0, 0.0, 0.0, 1.0);
            self.gl.clear(glow::COLOR_BUFFER_BIT);
            let scale = (fb_w as f32 / vw.max(1) as f32).min(fb_h as f32 / vh.max(1) as f32);
            let (dw, dh) = ((vw as f32 * scale) as i32, (vh as f32 * scale) as i32);
            self.gl.viewport((fb_w - dw) / 2, (fb_h - dh) / 2, dw, dh);
        }
    }

    fn import(&self, display: egl::Display, p: &DmabufPlane) -> Result<egl::Image> {
        // Buffers are always linear (modifier 0), so we omit the modifier
        // attribs, which would need EGL_EXT_image_dma_buf_import_modifiers.
        let attribs: [egl::Attrib; 13] = [
            EGL_WIDTH, p.width as egl::Attrib,
            EGL_HEIGHT, p.height as egl::Attrib,
            EGL_LINUX_DRM_FOURCC_EXT, p.fourcc as u32 as egl::Attrib,
            EGL_DMA_BUF_PLANE0_FD_EXT, p.fd.as_raw_fd() as egl::Attrib,
            EGL_DMA_BUF_PLANE0_OFFSET_EXT, p.offset as egl::Attrib,
            EGL_DMA_BUF_PLANE0_PITCH_EXT, p.stride as egl::Attrib,
            egl::ATTRIB_NONE,
        ];
        let _ = p.modifier;
        // SAFETY: NO_CONTEXT and a null client buffer are the required args for
        // an EGL_LINUX_DMABUF_EXT image; the attrib list is NONE-terminated and
        // the fd stays owned by the caller.
        let (ctx, buffer) = unsafe { (egl::Context::from_ptr(egl::NO_CONTEXT), egl::ClientBuffer::from_ptr(std::ptr::null_mut())) };
        self.egl
            .create_image(display, ctx, EGL_LINUX_DMABUF_EXT, buffer, &attribs)
            .map_err(|e| Error::Egl(format!("dmabuf import ({:?}): {e}", p.fourcc)))
    }

    fn set_i32(&self, program: glow::Program, name: &str, value: i32) {
        // SAFETY: called only between use_program and draw; missing uniform → None.
        unsafe {
            if let Some(loc) = self.gl.get_uniform_location(program, name) {
                self.gl.uniform_1_i32(Some(&loc), value);
            }
        }
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        // SAFETY: destroying our own GL objects. The owner clears the renderer
        // on GLArea unrealize with the context made current; on window teardown
        // the calls are harmless.
        unsafe {
            self.gl.delete_program(self.recombine);
            self.gl.delete_program(self.blit);
            self.gl.delete_vertex_array(self.vao);
            for t in self.plane_textures {
                self.gl.delete_texture(t);
            }
            self.gl.delete_texture(self.rgba_texture);
        }
    }
}

/// # Safety
/// A current GL context.
pub(crate) unsafe fn new_texture(gl: &glow::Context) -> Result<glow::NativeTexture> {
    // SAFETY: create + configure a texture on the current context.
    unsafe {
        let t = gl.create_texture().map_err(Error::Gl)?;
        gl.bind_texture(GL_TEXTURE_2D, Some(t));
        gl.tex_parameter_i32(GL_TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::NEAREST as i32);
        gl.tex_parameter_i32(GL_TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::NEAREST as i32);
        gl.tex_parameter_i32(GL_TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
        gl.tex_parameter_i32(GL_TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);
        Ok(t)
    }
}

pub(crate) const VERT: &str = r#"#version 300 es
const vec2 verts[3] = vec2[3](vec2(-1.0,-1.0), vec2(3.0,-1.0), vec2(-1.0,3.0));
out vec2 uv;
void main() {
    vec2 p = verts[gl_VertexID];
    uv = vec2((p.x + 1.0) * 0.5, 1.0 - (p.y + 1.0) * 0.5);
    gl_Position = vec4(p, 0.0, 1.0);
}
"#;

// Reconstructs 4:4:4 from the AVC444 layout (see haver-proto::chroma) and
// converts BT.709 limited-range YUV to RGB, matching yuv444_to_bgra.
const FRAG_RECOMBINE: &str = r#"#version 300 es
precision highp float;
precision highp int;
in vec2 uv;
out vec4 frag;
uniform sampler2D main_y;
uniform sampler2D main_uv;
uniform sampler2D aux_y;
uniform sampler2D aux_uv;
uniform int has_aux;
uniform int tex_w;
uniform int tex_h;
void main() {
    int px = clamp(int(uv.x * float(tex_w)), 0, tex_w - 1);
    int py = clamp(int(uv.y * float(tex_h)), 0, tex_h - 1);
    float Y = texelFetch(main_y, ivec2(px, py), 0).r;
    float U; float V;
    if (has_aux == 1) {
        if ((py & 1) == 0) {
            int bx = px >> 1; int by = py >> 1;
            vec2 c = ((px & 1) == 0) ? texelFetch(main_uv, ivec2(bx, by), 0).rg
                                     : texelFetch(aux_uv, ivec2(bx, by), 0).rg;
            U = c.r; V = c.g;
        } else {
            int by = py >> 1;
            U = texelFetch(aux_y, ivec2(px, by), 0).r;
            V = texelFetch(aux_y, ivec2(px, (tex_h / 2) + by), 0).r;
        }
    } else {
        vec2 c = texelFetch(main_uv, ivec2(px >> 1, py >> 1), 0).rg;
        U = c.r; V = c.g;
    }
    float y = Y * 255.0 - 16.0;
    float cb = U * 255.0 - 128.0;
    float cr = V * 255.0 - 128.0;
    float r = 1.1644 * y + 1.7927 * cr;
    float g = 1.1644 * y - 0.2132 * cb - 0.5329 * cr;
    float b = 1.1644 * y + 2.1124 * cb;
    frag = vec4(clamp(vec3(r, g, b) / 255.0, 0.0, 1.0), 1.0);
}
"#;

// Blit a BGRA texture (uploaded as RGBA, so swap R and B) to RGB.
const FRAG_BLIT: &str = r#"#version 300 es
precision highp float;
in vec2 uv;
out vec4 frag;
uniform sampler2D tex;
void main() {
    vec4 t = texture(tex, uv);
    frag = vec4(t.b, t.g, t.r, 1.0);
}
"#;

/// # Safety
/// A current GL context.
pub(crate) unsafe fn build_program(gl: &glow::Context, vert: &str, frag: &str) -> Result<glow::Program> {
    // SAFETY: standard shader compile/link on the current context.
    unsafe {
        let program = gl.create_program().map_err(Error::Gl)?;
        for (kind, src) in [(glow::VERTEX_SHADER, vert), (glow::FRAGMENT_SHADER, frag)] {
            let shader = gl.create_shader(kind).map_err(Error::Gl)?;
            gl.shader_source(shader, src);
            gl.compile_shader(shader);
            if !gl.get_shader_compile_status(shader) {
                return Err(Error::Gl(format!("shader compile: {}", gl.get_shader_info_log(shader))));
            }
            gl.attach_shader(program, shader);
            gl.delete_shader(shader);
        }
        gl.link_program(program);
        if !gl.get_program_link_status(program) {
            return Err(Error::Gl(format!("program link: {}", gl.get_program_info_log(program))));
        }
        Ok(program)
    }
}
