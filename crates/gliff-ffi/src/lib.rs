//! The C ABI the macOS client links: a gliff-client session whose decoder
//! and status sink are C callbacks, plus the input senders and the shared
//! geometry helpers. `cbindgen` turns this file into `gliff.h`.
//!
//! Threading and lifetime contract, also stated in the header:
//!
//! - `gliff_session_start` spawns a worker thread. Every callback runs on
//!   that thread, in order. Pointers passed to a callback are valid only
//!   until it returns; copy what you keep.
//! - `gliff_session_stop` stops the session, joins the worker, and frees
//!   the handle. When it returns no callback is running or will run again,
//!   so the caller may free `ctx`. Calling it from inside a callback would
//!   deadlock, so it aborts instead.
//! - The `gliff_send_*` functions may be called from any thread while the
//!   handle is live. They are no-ops once the session has ended.

use std::collections::BTreeSet;
use std::ffi::{c_char, c_void, CStr};
use std::sync::{Mutex, Once};
use std::thread::{JoinHandle, ThreadId};

use gliff_client::{Config, Endpoint, FrameRect, Input, Sink, SshTarget, Status, StopHandle};
use gliff_proto::{Axis, ChromaMode, ClientCaps, ClientMsg, Codec, CLIPBOARD_MAX};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

/// How to reach the server and what this client can do.
#[repr(C)]
pub struct GliffConfig {
    /// ssh destination (`user@host` or an ssh config alias), or null.
    pub host: *const c_char,
    /// Dev only: a `gliff-server --listen` address, used instead of `host`.
    pub tcp: *const c_char,
    /// Remote `gliff-server` path, or null for `gliff-server` on PATH.
    pub server_bin: *const c_char,
    /// Extra `gliff-server` arguments, e.g. `--headless`.
    pub server_args: *const *const c_char,
    pub server_args_len: usize,
    /// Extra ssh options.
    pub ssh_args: *const *const c_char,
    pub ssh_args_len: usize,
    /// Keymap for the server (xkb text or `rmlvo:` names), or null for its
    /// default.
    pub keymap: *const c_char,
    /// Largest stream the client will show, in pixels.
    pub max_width: u32,
    pub max_height: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GliffStatusKind {
    /// `width`, `height` and `scale_milli` describe the new stream.
    Connected,
    /// `fps`, `mbit` and `decode_ms` over the last second.
    Stats,
    /// A `width` x `height` BGRA cursor image in `data`, hotspot `hot_x`,
    /// `hot_y`.
    Cursor,
    /// The remote clipboard's text, UTF-8, in `data`.
    Clipboard,
    /// A line of ssh or server stderr, UTF-8, in `data`.
    Log,
    /// The session failed; the message is in `data`. The last status.
    Error,
    /// The server ended the session. The last status.
    Closed,
}

/// One status report. `data` (not NUL-terminated) is valid only during the
/// callback.
#[repr(C)]
pub struct GliffStatus {
    pub kind: GliffStatusKind,
    pub width: u32,
    pub height: u32,
    pub scale_milli: u32,
    pub hot_x: i32,
    pub hot_y: i32,
    pub fps: f32,
    pub mbit: f32,
    pub decode_ms: f32,
    pub data: *const u8,
    pub len: usize,
}

/// `decode` returns this when it showed a picture.
pub const GLIFF_DECODE_SHOWN: i32 = 1;
/// `decode` returns this when the access units produced no picture.
pub const GLIFF_DECODE_NONE: i32 = 0;
/// Any negative `decode` result is an error: the server is asked for a
/// keyframe.
pub const GLIFF_DECODE_ERROR: i32 = -1;

/// The platform side, called on the worker thread.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct GliffCallbacks {
    pub ctx: *mut c_void,
    /// A new stream of `width` x `height`; `dual` is true for 4:4:4 (a main
    /// and an auxiliary stream). Return false if no decoder can be made.
    pub configure: Option<unsafe extern "C" fn(*mut c_void, u32, u32, bool) -> bool>,
    /// Decode and show one frame: Annex B access units for the main and (when
    /// `aux_len` > 0) auxiliary streams. Returns a `GLIFF_DECODE_*` value.
    /// The frame is acked when this returns.
    pub decode:
        Option<unsafe extern "C" fn(*mut c_void, *const u8, usize, *const u8, usize) -> i32>,
    pub status: Option<unsafe extern "C" fn(*mut c_void, *const GliffStatus)>,
}

/// The callbacks move to the worker thread and are only called there.
struct CallbackSink(GliffCallbacks);

// SAFETY: the header requires `ctx` and the callbacks to be usable from the
// worker thread; this crate never calls them from anywhere else.
unsafe impl Send for CallbackSink {}

impl CallbackSink {
    fn report(&mut self, status: GliffStatus) {
        if let Some(f) = self.0.status {
            // SAFETY: a callback the caller supplied, with its own `ctx`; the
            // status and its data outlive the call.
            unsafe { f(self.0.ctx, &status) }
        }
    }
}

fn status(kind: GliffStatusKind) -> GliffStatus {
    GliffStatus {
        kind,
        width: 0,
        height: 0,
        scale_milli: 0,
        hot_x: 0,
        hot_y: 0,
        fps: 0.0,
        mbit: 0.0,
        decode_ms: 0.0,
        data: std::ptr::null(),
        len: 0,
    }
}

fn with_data(kind: GliffStatusKind, data: &[u8]) -> GliffStatus {
    GliffStatus {
        data: data.as_ptr(),
        len: data.len(),
        ..status(kind)
    }
}

impl Sink for CallbackSink {
    type Frame = ();

    fn configure(&mut self, width: u32, height: u32, chroma: ChromaMode) -> anyhow::Result<()> {
        let Some(f) = self.0.configure else {
            anyhow::bail!("no configure callback");
        };
        // SAFETY: as in `report`.
        let ok = unsafe { f(self.0.ctx, width, height, chroma != ChromaMode::Single420) };
        anyhow::ensure!(ok, "the client could not create a {width}x{height} decoder");
        Ok(())
    }

    fn decode(&mut self, main: &[u8], aux: &[u8]) -> anyhow::Result<Option<()>> {
        let Some(f) = self.0.decode else {
            anyhow::bail!("no decode callback");
        };
        // SAFETY: as in `report`; both slices outlive the call.
        let result = unsafe {
            f(
                self.0.ctx,
                main.as_ptr(),
                main.len(),
                aux.as_ptr(),
                aux.len(),
            )
        };
        match result {
            GLIFF_DECODE_SHOWN => Ok(Some(())),
            r if r >= 0 => Ok(None),
            r => anyhow::bail!("decode failed ({r})"),
        }
    }

    // `decode` already showed the picture.
    fn frame(&mut self, _: ()) {}

    fn status(&mut self, s: Status) {
        use GliffStatusKind as K;
        match s {
            Status::Connected {
                width,
                height,
                scale_milli,
            } => self.report(GliffStatus {
                width,
                height,
                scale_milli,
                ..status(K::Connected)
            }),
            Status::Stats {
                fps,
                mbit,
                decode_ms,
            } => self.report(GliffStatus {
                fps,
                mbit,
                decode_ms,
                ..status(K::Stats)
            }),
            Status::Cursor {
                width,
                height,
                hot_x,
                hot_y,
                argb,
            } => self.report(GliffStatus {
                width,
                height,
                hot_x,
                hot_y,
                ..with_data(K::Cursor, &argb)
            }),
            Status::Clipboard(text) => self.report(with_data(K::Clipboard, text.as_bytes())),
            Status::Log(line) => self.report(with_data(K::Log, line.as_bytes())),
            Status::Error(message) => self.report(with_data(K::Error, message.as_bytes())),
            Status::Closed => self.report(status(K::Closed)),
        }
    }
}

/// A running session. Opaque to C.
pub struct GliffSession {
    input: UnboundedSender<Input>,
    stop: StopHandle,
    worker: JoinHandle<()>,
    worker_id: ThreadId,
    held: Mutex<Held>,
}

/// Keys and buttons pressed on the remote, so all of them can be released
/// when the local window loses the keyboard.
#[derive(Default)]
struct Held {
    keys: BTreeSet<u32>,
    buttons: BTreeSet<u32>,
}

impl GliffSession {
    fn send(&self, msg: ClientMsg) {
        let _ = self.input.send((msg, Vec::new()));
    }
}

/// Read a C string; null is `None`.
///
/// # Safety
/// `p` is null or a valid NUL-terminated string.
unsafe fn string(p: *const c_char) -> Option<String> {
    if p.is_null() {
        return None;
    }
    // SAFETY: non-null, and the caller promises a C string.
    Some(unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned())
}

/// # Safety
/// `p` points to `len` valid C strings (or `len` is 0).
unsafe fn strings(p: *const *const c_char, len: usize) -> Vec<String> {
    if p.is_null() || len == 0 {
        return Vec::new();
    }
    // SAFETY: the caller promises `len` entries.
    let items = unsafe { std::slice::from_raw_parts(p, len) };
    items
        .iter()
        // SAFETY: each entry is a C string, per the caller.
        .filter_map(|&s| unsafe { string(s) })
        .collect()
}

/// Start a session on a new worker thread. Returns null if the config names
/// no endpoint.
///
/// # Safety
/// `config` points to a valid `GliffConfig` whose strings are valid for this
/// call. `callbacks.ctx` must stay valid until `gliff_session_stop` returns.
#[no_mangle]
pub unsafe extern "C" fn gliff_session_start(
    config: *const GliffConfig,
    callbacks: GliffCallbacks,
) -> *mut GliffSession {
    if config.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: non-null, and the caller promises a valid config.
    let c = unsafe { &*config };
    // SAFETY: the config's strings are valid, per the caller.
    let (tcp, host, server_bin, server_args, ssh_args, keymap) = unsafe {
        (
            string(c.tcp),
            string(c.host),
            string(c.server_bin),
            strings(c.server_args, c.server_args_len),
            strings(c.ssh_args, c.ssh_args_len),
            string(c.keymap).unwrap_or_default(),
        )
    };
    let endpoint = match (
        tcp.filter(|s| !s.is_empty()),
        host.filter(|s| !s.is_empty()),
    ) {
        (Some(addr), _) => Endpoint::Tcp(addr),
        (None, Some(host)) => {
            let mut target = SshTarget::new(host);
            if let Some(bin) = server_bin.filter(|s| !s.is_empty()) {
                target.server_bin = bin;
            }
            target.server_args = server_args;
            target.ssh_args = ssh_args;
            target.capture_stderr = true;
            Endpoint::Ssh(target)
        }
        (None, None) => return std::ptr::null_mut(),
    };
    let config = Config {
        endpoint,
        keymap,
        caps: ClientCaps {
            codecs: vec![Codec::H264],
            max_width: c.max_width.max(640),
            max_height: c.max_height.max(480),
            chroma: vec![ChromaMode::Dual420, ChromaMode::Single420],
        },
    };

    let (input_tx, input_rx) = unbounded_channel();
    let (stop, signal) = gliff_client::stopper();
    let sink = CallbackSink(callbacks);
    let worker = match std::thread::Builder::new()
        .name("gliff-session".into())
        .spawn(move || gliff_client::run(config, sink, input_rx, signal))
    {
        Ok(worker) => worker,
        Err(_) => return std::ptr::null_mut(),
    };
    let worker_id = worker.thread().id();
    Box::into_raw(Box::new(GliffSession {
        input: input_tx,
        stop,
        worker,
        worker_id,
        held: Mutex::new(Held::default()),
    }))
}

/// Stop the session, wait for its worker, and free the handle. No callback
/// runs after this returns. Null is ignored.
///
/// # Safety
/// `session` is null or a handle from `gliff_session_start` that has not
/// been stopped. Must not be called from inside a callback.
#[no_mangle]
pub unsafe extern "C" fn gliff_session_stop(session: *mut GliffSession) {
    if session.is_null() {
        return;
    }
    // SAFETY: a live handle from `gliff_session_start`, per the caller; this
    // takes it back.
    let session = unsafe { Box::from_raw(session) };
    if std::thread::current().id() == session.worker_id {
        eprintln!("gliff_session_stop called from a gliff callback; that would deadlock");
        std::process::abort();
    }
    session.stop.stop();
    let _ = session.worker.join();
}

/// Borrow a live session.
///
/// # Safety
/// `session` is null or a live handle.
unsafe fn live<'a>(session: *const GliffSession) -> Option<&'a GliffSession> {
    // SAFETY: per the caller.
    unsafe { session.as_ref() }
}

/// Press or release a key, by Linux evdev code. Returns false when the
/// event was not sent: a press of a key already down (auto-repeat, which the
/// remote does itself) or a release of a key that is not down.
///
/// # Safety
/// `session` is null or a live handle.
#[no_mangle]
pub unsafe extern "C" fn gliff_send_key(
    session: *const GliffSession,
    keycode: u32,
    pressed: bool,
) -> bool {
    // SAFETY: per the caller.
    let Some(s) = (unsafe { live(session) }) else {
        return false;
    };
    let changed = {
        let mut held = s.held.lock().unwrap();
        if pressed {
            held.keys.insert(keycode)
        } else {
            held.keys.remove(&keycode)
        }
    };
    if changed {
        s.send(ClientMsg::Key { keycode, pressed });
    }
    changed
}

/// Move the pointer, in the remote output's logical coordinates (see
/// `gliff_to_remote`).
///
/// # Safety
/// `session` is null or a live handle.
#[no_mangle]
pub unsafe extern "C" fn gliff_send_pointer_motion(session: *const GliffSession, x: f64, y: f64) {
    // SAFETY: per the caller.
    if let Some(s) = unsafe { live(session) } {
        s.send(ClientMsg::PointerMotion { x, y });
    }
}

/// Press or release a pointer button, by Linux `BTN_*` code.
///
/// # Safety
/// `session` is null or a live handle.
#[no_mangle]
pub unsafe extern "C" fn gliff_send_pointer_button(
    session: *const GliffSession,
    button: u32,
    pressed: bool,
) {
    // SAFETY: per the caller.
    let Some(s) = (unsafe { live(session) }) else {
        return;
    };
    let changed = {
        let mut held = s.held.lock().unwrap();
        if pressed {
            held.buttons.insert(button)
        } else {
            held.buttons.remove(&button)
        }
    };
    if changed {
        s.send(ClientMsg::PointerButton { button, pressed });
    }
}

/// Scroll. `value` is in the remote's scroll units (about a pixel; positive
/// scrolls down or right). A wheel sets `has_discrete` and gives notches in
/// `discrete`; a trackpad sends continuous values and `stop` when the finger
/// lifts.
///
/// # Safety
/// `session` is null or a live handle.
#[no_mangle]
pub unsafe extern "C" fn gliff_send_axis(
    session: *const GliffSession,
    horizontal: bool,
    value: f64,
    has_discrete: bool,
    discrete: i32,
    stop: bool,
) {
    // SAFETY: per the caller.
    if let Some(s) = unsafe { live(session) } {
        s.send(ClientMsg::PointerAxis {
            axis: if horizontal {
                Axis::Horizontal
            } else {
                Axis::Vertical
            },
            value,
            discrete: has_discrete.then_some(discrete),
            stop,
        });
    }
}

/// Release every key and button this client holds down on the remote. Call
/// it when the window loses the keyboard, or their releases never arrive.
///
/// # Safety
/// `session` is null or a live handle.
#[no_mangle]
pub unsafe extern "C" fn gliff_release_all_input(session: *const GliffSession) {
    // SAFETY: per the caller.
    let Some(s) = (unsafe { live(session) }) else {
        return;
    };
    let held = std::mem::take(&mut *s.held.lock().unwrap());
    for keycode in held.keys {
        s.send(ClientMsg::Key {
            keycode,
            pressed: false,
        });
    }
    for button in held.buttons {
        s.send(ClientMsg::PointerButton {
            button,
            pressed: false,
        });
    }
}

/// Ask for a stream of `width` x `height` pixels at `scale` device pixels
/// per point: a headless remote output follows it, a mirrored one is
/// letterboxed.
///
/// # Safety
/// `session` is null or a live handle.
#[no_mangle]
pub unsafe extern "C" fn gliff_send_resize(
    session: *const GliffSession,
    width: u32,
    height: u32,
    scale: f32,
) {
    // SAFETY: per the caller.
    if let Some(s) = unsafe { live(session) } {
        s.send(ClientMsg::Resize {
            width,
            height,
            scale,
        });
    }
}

/// Put UTF-8 text on the remote clipboard.
///
/// # Safety
/// `session` is null or a live handle; `text` points to `len` bytes.
#[no_mangle]
pub unsafe extern "C" fn gliff_send_clipboard_text(
    session: *const GliffSession,
    text: *const u8,
    len: usize,
) {
    // SAFETY: per the caller.
    let Some(s) = (unsafe { live(session) }) else {
        return;
    };
    if text.is_null() || len as u64 > CLIPBOARD_MAX {
        return;
    }
    // SAFETY: `len` readable bytes, per the caller.
    let bytes = unsafe { std::slice::from_raw_parts(text, len) }.to_vec();
    let _ = s.input.send((
        ClientMsg::ClipboardData {
            mime_type: "text/plain;charset=utf-8".into(),
            offset: 0,
            total: len as u64,
            data_len: len as u32,
        },
        bytes,
    ));
}

/// A rectangle in view units.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GliffRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// Where a `stream_width` x `stream_height` frame is drawn in a view of
/// `view_width` x `view_height` points at `device_scale` pixels per point:
/// 1:1 when it fits, shrunk when it does not, centred. The origin is the
/// view's top left.
#[no_mangle]
pub extern "C" fn gliff_frame_rect(
    stream_width: u32,
    stream_height: u32,
    device_scale: f64,
    view_width: f64,
    view_height: f64,
) -> GliffRect {
    let r = FrameRect::new(
        stream_width,
        stream_height,
        device_scale,
        view_width,
        view_height,
    );
    GliffRect {
        x: r.x,
        y: r.y,
        width: r.width,
        height: r.height,
    }
}

/// Map a view point (top-left origin) to the remote's logical coordinates,
/// for `gliff_send_pointer_motion`. `remote_scale` is the stream's
/// `scale_milli / 1000`.
///
/// # Safety
/// `out_x` and `out_y` are valid for writes.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn gliff_to_remote(
    stream_width: u32,
    stream_height: u32,
    device_scale: f64,
    view_width: f64,
    view_height: f64,
    remote_scale: f64,
    x: f64,
    y: f64,
    out_x: *mut f64,
    out_y: *mut f64,
) {
    let r = FrameRect::new(
        stream_width,
        stream_height,
        device_scale,
        view_width,
        view_height,
    );
    let (rx, ry) = r.to_remote(x, y, remote_scale);
    // SAFETY: valid for writes, per the caller.
    unsafe {
        *out_x = rx;
        *out_y = ry;
    }
}

/// True if a BGRA cursor image has a visible shape (see gliff-client).
///
/// # Safety
/// `argb` points to `len` bytes.
#[no_mangle]
pub unsafe extern "C" fn gliff_has_visible_shape(argb: *const u8, len: usize) -> bool {
    if argb.is_null() {
        return false;
    }
    // SAFETY: `len` readable bytes, per the caller.
    gliff_client::has_visible_shape(unsafe { std::slice::from_raw_parts(argb, len) })
}

/// Log to stderr, filtered by `RUST_LOG` (default `info`). Safe to call
/// more than once.
#[no_mangle]
pub extern "C" fn gliff_init_logging() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let filter = tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .try_init();
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static ERRORS: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn on_status(_: *mut c_void, status: *const GliffStatus) {
        // SAFETY: the session passes a valid status.
        if unsafe { (*status).kind } == GliffStatusKind::Error {
            ERRORS.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn a_session_with_no_endpoint_is_refused() {
        let config = GliffConfig {
            host: std::ptr::null(),
            tcp: std::ptr::null(),
            server_bin: std::ptr::null(),
            server_args: std::ptr::null(),
            server_args_len: 0,
            ssh_args: std::ptr::null(),
            ssh_args_len: 0,
            keymap: std::ptr::null(),
            max_width: 1920,
            max_height: 1080,
        };
        let callbacks = GliffCallbacks {
            ctx: std::ptr::null_mut(),
            configure: None,
            decode: None,
            status: None,
        };
        // SAFETY: a valid config.
        assert!(unsafe { gliff_session_start(&config, callbacks) }.is_null());
    }

    #[test]
    fn a_refused_connection_reports_an_error_and_input_is_tracked() {
        // Nothing listens on this port once the listener is dropped.
        let addr = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().to_string()
        };
        let tcp = CString::new(addr).unwrap();
        let config = GliffConfig {
            host: std::ptr::null(),
            tcp: tcp.as_ptr(),
            server_bin: std::ptr::null(),
            server_args: std::ptr::null(),
            server_args_len: 0,
            ssh_args: std::ptr::null(),
            ssh_args_len: 0,
            keymap: std::ptr::null(),
            max_width: 1920,
            max_height: 1080,
        };
        let callbacks = GliffCallbacks {
            ctx: std::ptr::null_mut(),
            configure: None,
            decode: None,
            status: Some(on_status),
        };
        // SAFETY: valid config and callbacks.
        let session = unsafe { gliff_session_start(&config, callbacks) };
        assert!(!session.is_null());
        // SAFETY: a live session.
        unsafe {
            assert!(gliff_send_key(session, 30, true));
            assert!(!gliff_send_key(session, 30, true), "repeat is not resent");
            gliff_release_all_input(session);
            assert!(!gliff_send_key(session, 30, false), "already released");
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
        // SAFETY: live, and not called from a callback.
        unsafe { gliff_session_stop(session) };
        assert_eq!(ERRORS.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn geometry_matches_gliff_client() {
        let r = gliff_frame_rect(2000, 1000, 2.0, 1200.0, 700.0);
        assert_eq!(
            r,
            GliffRect {
                x: 100.0,
                y: 100.0,
                width: 1000.0,
                height: 500.0
            }
        );
        let (mut x, mut y) = (0.0, 0.0);
        // SAFETY: valid out pointers.
        unsafe {
            gliff_to_remote(
                2000, 1000, 2.0, 1200.0, 700.0, 2.0, 600.0, 350.0, &mut x, &mut y,
            )
        };
        assert_eq!((x, y), (500.0, 250.0));
    }
}
