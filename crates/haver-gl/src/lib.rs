//! GPU path: import decoded NV12 dmabufs as textures and recombine AVC444 4:4:4
//! into RGB in one shader pass, replacing the client's CPU recombine + colour.
//!
//! This crate is the ONLY place in haver that uses `unsafe`, because GL and EGL
//! are C APIs: `glow` marks every GL call `unsafe`, EGL image import needs raw
//! pointers, and the `glEGLImageTargetTexture2DOES` entry point is loaded at
//! run time. Every `unsafe` block has a `// SAFETY:` comment. The safe facade
//! is `supports_dmabuf_import()` and `Renderer`.

use std::ffi::{c_void, CString};
use std::os::fd::{AsRawFd, BorrowedFd};

use drm_fourcc::DrmFourcc;
use glow::HasContext;
use khronos_egl as egl;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("EGL: {0}")]
    Egl(String),
    #[error("GL: {0}")]
    Gl(String),
    #[error("EGL_EXT_image_dma_buf_import is not available")]
    NoDmabufImport,
    #[error("no current EGL display (call inside a current GL context)")]
    NoDisplay,
}

pub type Result<T> = std::result::Result<T, Error>;

// EGL dmabuf-import attribute names (from EGL_EXT_image_dma_buf_import).
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

/// A single dmabuf plane to import as one texture.
pub struct DmabufPlane<'a> {
    pub fd: BorrowedFd<'a>,
    pub width: u32,
    pub height: u32,
    pub offset: u32,
    pub stride: u32,
    /// DRM fourcc, e.g. `DrmFourcc::R8` for a luma plane, `Gr88` for NV12 UV.
    pub fourcc: DrmFourcc,
    pub modifier: u64,
}

/// The four planes of an AVC444 frame: main and (for 4:4:4) auxiliary NV12.
pub struct FramePlanes<'a> {
    pub main_y: DmabufPlane<'a>,
    pub main_uv: DmabufPlane<'a>,
    /// `None` for the single-stream (4:2:0) path.
    pub aux: Option<(DmabufPlane<'a>, DmabufPlane<'a>)>,
    pub width: u32,
    pub height: u32,
}

/// True if this machine's EGL can import dmabufs as textures. Safe to call
/// before any GL context exists; used to decide the GPU vs CPU display path.
pub fn supports_dmabuf_import() -> bool {
    probe().unwrap_or(false)
}

fn probe() -> Result<bool> {
    // Query the extension on a GBM display over the render node, which is the
    // same EGL platform GTK uses, so the answer matches the real context.
    let file = std::fs::File::options()
        .read(true)
        .write(true)
        .open("/dev/dri/renderD128")
        .map_err(|e| Error::Egl(format!("open render node: {e}")))?;
    let device = gbm::Device::new(file).map_err(|e| Error::Egl(format!("gbm: {e}")))?;
    let egl = egl::Instance::new(egl::Static);
    use gbm::AsRaw;
    // SAFETY: the gbm device pointer is a valid EGL native display on Mesa; the
    // Display it yields is only used to initialize and query strings.
    let display = unsafe { egl.get_display(device.as_raw() as *mut std::ffi::c_void) }.ok_or(Error::NoDisplay)?;
    egl.initialize(display).map_err(|e| Error::Egl(e.to_string()))?;
    let exts = egl.query_string(Some(display), egl::EXTENSIONS).map_err(|e| Error::Egl(e.to_string()))?;
    let ok = exts.to_string_lossy().contains("EGL_EXT_image_dma_buf_import");
    let _ = egl.terminate(display);
    Ok(ok)
}

type ImageTargetTexture2D = extern "system" fn(u32, *mut c_void);

/// Recombines decoded NV12 dmabufs into RGB with one shader pass. Create it
/// inside a current GL context (e.g. a `gtk::GLArea` render callback).
pub struct Renderer {
    egl: egl::Instance<egl::Static>,
    gl: glow::Context,
    program: glow::Program,
    vao: glow::VertexArray,
    textures: [glow::NativeTexture; 4],
    image_target: ImageTargetTexture2D,
}

impl Renderer {
    /// Build the recombine program in the current GL context.
    pub fn new() -> Result<Self> {
        let egl = egl::Instance::new(egl::Static);
        let display = egl.get_current_display().ok_or(Error::NoDisplay)?;
        let exts = egl.query_string(Some(display), egl::EXTENSIONS).map_err(|e| Error::Egl(e.to_string()))?;
        if !exts.to_string_lossy().contains("EGL_EXT_image_dma_buf_import") {
            return Err(Error::NoDmabufImport);
        }
        let target_name = CString::new("glEGLImageTargetTexture2DOES").unwrap();
        let raw = egl.get_proc_address(target_name.to_str().unwrap()).ok_or_else(|| Error::Gl("glEGLImageTargetTexture2DOES missing".into()))?;
        // SAFETY: the function pointer for glEGLImageTargetTexture2DOES has the
        // signature (GLenum target, GLeglImageOES image); we transmute the
        // untyped EGL entry point to exactly that.
        let image_target: ImageTargetTexture2D = unsafe { std::mem::transmute(raw) };

        // SAFETY: `get_proc_address` returns valid GL entry points for the
        // current context; glow only calls them while that context is current.
        let gl = unsafe {
            glow::Context::from_loader_function(|s| match egl.get_proc_address(s) {
                Some(f) => f as *const c_void,
                None => std::ptr::null(),
            })
        };

        // SAFETY: standard GL object creation on the current context; each call
        // is checked and the objects are stored for the renderer's lifetime.
        let (program, vao, textures) = unsafe {
            let program = build_program(&gl)?;
            let vao = gl.create_vertex_array().map_err(Error::Gl)?;
            let mut textures = [glow::NativeTexture(std::num::NonZeroU32::new(1).unwrap()); 4];
            for t in &mut textures {
                *t = gl.create_texture().map_err(Error::Gl)?;
                gl.bind_texture(GL_TEXTURE_2D, Some(*t));
                gl.tex_parameter_i32(GL_TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::NEAREST as i32);
                gl.tex_parameter_i32(GL_TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::NEAREST as i32);
                gl.tex_parameter_i32(GL_TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
                gl.tex_parameter_i32(GL_TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);
            }
            (program, vao, textures)
        };
        Ok(Self { egl, gl, program, vao, textures, image_target })
    }

    /// Draw one frame into the current framebuffer, letterboxed (Contain) to
    /// `fb_w`x`fb_h`.
    pub fn draw(&self, planes: &FramePlanes, fb_w: i32, fb_h: i32) -> Result<()> {
        let display = self.egl.get_current_display().ok_or(Error::NoDisplay)?;
        let (vw, vh) = (planes.width as i32, planes.height as i32);

        // Import each plane as an EGLImage bound to a texture.
        let mut images = Vec::new();
        let bind = |unit: usize, plane: &DmabufPlane| -> Result<egl::Image> {
            let image = self.import(display, plane)?;
            // SAFETY: `unit` < 4 and the texture handles are valid; the OES
            // entry point binds the EGLImage to the currently bound 2D texture.
            unsafe {
                self.gl.active_texture(glow::TEXTURE0 + unit as u32);
                self.gl.bind_texture(GL_TEXTURE_2D, Some(self.textures[unit]));
                (self.image_target)(GL_TEXTURE_2D, image.as_ptr());
            }
            Ok(image)
        };
        images.push(bind(0, &planes.main_y)?);
        images.push(bind(1, &planes.main_uv)?);
        let has_aux = planes.aux.is_some();
        if let Some((ay, auv)) = &planes.aux {
            images.push(bind(2, ay)?);
            images.push(bind(3, auv)?);
        }

        // Contain letterbox: viewport keeps the video's aspect ratio.
        let scale = (fb_w as f32 / vw as f32).min(fb_h as f32 / vh as f32);
        let (dw, dh) = ((vw as f32 * scale) as i32, (vh as f32 * scale) as i32);
        let (ox, oy) = ((fb_w - dw) / 2, (fb_h - dh) / 2);

        // SAFETY: uniform locations and enums are valid for `self.program`; the
        // draw covers a single triangle with no external buffers.
        unsafe {
            self.gl.viewport(0, 0, fb_w, fb_h);
            self.gl.clear_color(0.0, 0.0, 0.0, 1.0);
            self.gl.clear(glow::COLOR_BUFFER_BIT);
            self.gl.viewport(ox, oy, dw, dh);
            self.gl.use_program(Some(self.program));
            self.set_i32("main_y", 0);
            self.set_i32("main_uv", 1);
            self.set_i32("aux_y", 2);
            self.set_i32("aux_uv", 3);
            self.set_i32("has_aux", has_aux as i32);
            self.set_i32("tex_w", vw);
            self.set_i32("tex_h", vh);
            self.gl.bind_vertex_array(Some(self.vao));
            self.gl.draw_arrays(glow::TRIANGLES, 0, 3);
            self.gl.bind_vertex_array(None);
        }

        for image in images {
            let _ = self.egl.destroy_image(display, image);
        }
        Ok(())
    }

    fn import(&self, display: egl::Display, p: &DmabufPlane) -> Result<egl::Image> {
        let attribs: [egl::Attrib; 17] = [
            EGL_WIDTH, p.width as egl::Attrib,
            EGL_HEIGHT, p.height as egl::Attrib,
            EGL_LINUX_DRM_FOURCC_EXT, p.fourcc as u32 as egl::Attrib,
            EGL_DMA_BUF_PLANE0_FD_EXT, p.fd.as_raw_fd() as egl::Attrib,
            EGL_DMA_BUF_PLANE0_OFFSET_EXT, p.offset as egl::Attrib,
            EGL_DMA_BUF_PLANE0_PITCH_EXT, p.stride as egl::Attrib,
            EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT, (p.modifier & 0xffff_ffff) as egl::Attrib,
            EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT, (p.modifier >> 32) as egl::Attrib,
            egl::ATTRIB_NONE,
        ];
        // SAFETY: NO_CONTEXT and a null client buffer are the required arguments
        // for an EGL_LINUX_DMABUF_EXT image; the attrib list is well-formed and
        // NONE-terminated. The imported fd stays owned by the caller.
        let (ctx, buffer) = unsafe { (egl::Context::from_ptr(egl::NO_CONTEXT), egl::ClientBuffer::from_ptr(std::ptr::null_mut())) };
        self.egl
            .create_image(display, ctx, EGL_LINUX_DMABUF_EXT, buffer, &attribs)
            .map_err(|e| Error::Egl(format!("dmabuf import ({:?}): {e}", p.fourcc)))
    }

    fn set_i32(&self, name: &str, value: i32) {
        // SAFETY: called only between use_program and draw; a missing uniform
        // yields None and is skipped.
        unsafe {
            if let Some(loc) = self.gl.get_uniform_location(self.program, name) {
                self.gl.uniform_1_i32(Some(&loc), value);
            }
        }
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        // SAFETY: destroying our own GL objects on the (still current) context.
        unsafe {
            self.gl.delete_program(self.program);
            self.gl.delete_vertex_array(self.vao);
            for t in self.textures {
                self.gl.delete_texture(t);
            }
        }
    }
}

const VERT: &str = r#"#version 300 es
const vec2 verts[3] = vec2[3](vec2(-1.0,-1.0), vec2(3.0,-1.0), vec2(-1.0,3.0));
out vec2 uv;
void main() {
    vec2 p = verts[gl_VertexID];
    // Map clip space to [0,1] with y flipped (texture row 0 is the top).
    uv = vec2((p.x + 1.0) * 0.5, 1.0 - (p.y + 1.0) * 0.5);
    gl_Position = vec4(p, 0.0, 1.0);
}
"#;

// Reconstructs 4:4:4 from the AVC444 layout (see haver-proto::chroma) and
// converts BT.709 limited-range YUV to RGB, matching yuv444_to_bgra.
const FRAG: &str = r#"#version 300 es
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
    int px = int(uv.x * float(tex_w));
    int py = int(uv.y * float(tex_h));
    px = clamp(px, 0, tex_w - 1);
    py = clamp(py, 0, tex_h - 1);

    float Y = texelFetch(main_y, ivec2(px, py), 0).r;
    float U;
    float V;
    if (has_aux == 1) {
        if ((py & 1) == 0) {
            int bx = px >> 1;
            int by = py >> 1;
            if ((px & 1) == 0) {
                vec2 c = texelFetch(main_uv, ivec2(bx, by), 0).rg;
                U = c.r; V = c.g;
            } else {
                vec2 c = texelFetch(aux_uv, ivec2(bx, by), 0).rg;
                U = c.r; V = c.g;
            }
        } else {
            int by = py >> 1;
            U = texelFetch(aux_y, ivec2(px, by), 0).r;
            V = texelFetch(aux_y, ivec2(px, (tex_h / 2) + by), 0).r;
        }
    } else {
        vec2 c = texelFetch(main_uv, ivec2(px >> 1, py >> 1), 0).rg;
        U = c.r; V = c.g;
    }

    // BT.709 limited-range YUV (0..1 unorm) -> RGB.
    float y = Y * 255.0 - 16.0;
    float cb = U * 255.0 - 128.0;
    float cr = V * 255.0 - 128.0;
    float r = 1.1644 * y + 1.7927 * cr;
    float g = 1.1644 * y - 0.2132 * cb - 0.5329 * cr;
    float b = 1.1644 * y + 2.1124 * cb;
    frag = vec4(clamp(vec3(r, g, b) / 255.0, 0.0, 1.0), 1.0);
}
"#;

/// # Safety
/// The current GL context must be valid.
unsafe fn build_program(gl: &glow::Context) -> Result<glow::Program> {
    // SAFETY: the caller guarantees a current GL context; every call below is a
    // standard GL object/shader operation on that context.
    unsafe {
        let program = gl.create_program().map_err(Error::Gl)?;
        for (kind, src) in [(glow::VERTEX_SHADER, VERT), (glow::FRAGMENT_SHADER, FRAG)] {
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
