//! haver-client: a GTK4/libadwaita window that connects to a haver server,
//! decodes the video, shows it, and forwards keyboard and pointer input.
//!
//! Decode runs on a worker thread (see `net`); this file is the UI. It has no
//! `unsafe`: all GL/EGL lives in the haver-gl crate, called from the GLArea
//! render callback.

mod keymap;
mod net;

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, sync_channel, Receiver};
use std::sync::Arc;
use std::time::{Duration, Instant};

use adw::prelude::*;
use clap::Parser;
use gtk4 as gtk;
use gtk::gdk;
use gtk::glib;
use haver_proto::{Axis, ClientMsg};
use haver_transport::SshTarget;
use libadwaita as adw;
use net::{DecodedFrame, Endpoint, Status, Worker};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

/// A message plus optional trailing payload, sent from the UI to the worker.
type OutSender = UnboundedSender<(ClientMsg, Vec<u8>)>;

#[derive(Parser)]
#[command(name = "haver-client", about = "Remote-desktop a Hyprland session over ssh")]
struct Cli {
    /// `user@host` to ssh to and spawn haver-server, or empty to type it in.
    host: Option<String>,
    /// Dev: connect directly to a `haver-server --listen` address.
    #[arg(long)]
    connect: Option<String>,
    /// Remote haver-server path.
    #[arg(long, default_value = "haver-server")]
    server_bin: String,
    /// Pass --headless to the remote server.
    #[arg(long, default_value_t = true)]
    headless: bool,
    /// Hotkey that releases captured shortcuts and hands the keyboard back to
    /// the local compositor. Forms: a chord like `shift+escape`, `ctrl+alt+q`
    /// or `super+escape`; `double-<key>` for a double-tap (e.g.
    /// `double-escape`); or `none` to disable. The screen recaptures when you
    /// click it again.
    #[arg(long, default_value = "shift+escape")]
    release_hotkey: String,
}

/// GPU display: a GLArea whose render callback recombines dmabuf planes, or
/// blits CPU BGRA when dmabuf import is unavailable.
struct GlView {
    area: gtk::GLArea,
    frame: Rc<RefCell<Option<net::DecodedFrame>>>,
}

/// Everything the UI shares with its callbacks.
struct App {
    /// The GLArea, upcast; input controllers attach to it and we measure it.
    video: gtk::Widget,
    gl: GlView,
    /// Shared with the worker: true = it sends dmabuf planes (GPU path), false =
    /// BGRA (CPU). The GLArea render callback clears it if GPU rendering fails,
    /// so a wrong capability guess self-corrects at run time.
    gpu: Arc<AtomicBool>,
    stats: gtk::Label,
    status: gtk::Label,
    stream_size: Rc<RefCell<(u32, u32)>>,
    input_tx: Rc<RefCell<Option<OutSender>>>,
    /// Last text we set on the local clipboard from the remote, to avoid echo.
    last_remote_clip: Rc<RefCell<Option<String>>>,
    /// The last endpoint, kept so a dropped connection can be retried.
    endpoint: Rc<RefCell<Option<Endpoint>>>,
    /// Consecutive failed connection attempts, reset on a successful connect.
    retries: Rc<RefCell<u32>>,
}

fn main() -> glib::ExitCode {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).init();
    let cli = Cli::parse();
    let app = adw::Application::builder().application_id("com.haver.Client").build();
    app.connect_activate(move |app| build_ui(app, &cli));
    // GTK owns argv parsing; we already parsed with clap, so pass none.
    let empty: Vec<String> = vec![];
    app.run_with_args(&empty)
}

fn build_ui(app: &adw::Application, cli: &Cli) {
    let window = adw::ApplicationWindow::builder().application(app).default_width(1280).default_height(760).build();

    let header = adw::HeaderBar::new();
    let host_entry = gtk::Entry::builder().placeholder_text("user@host").hexpand(true).build();
    if let Some(h) = &cli.host {
        host_entry.set_text(h);
    }
    let connect_btn = gtk::Button::with_label("Connect");
    let fullscreen_btn = gtk::ToggleButton::builder().icon_name("view-fullscreen-symbolic").build();
    header.pack_start(&host_entry);
    header.pack_start(&connect_btn);
    header.pack_end(&fullscreen_btn);

    // Probe whether EGL here can import dmabufs; that decides the initial path.
    // The video widget is always a GLArea: it recombines dmabuf planes on the
    // GPU when possible, and blits CPU BGRA otherwise. The `gpu` flag (shared
    // with the worker) is cleared by the render callback if GPU rendering fails,
    // so a wrong guess falls back to the CPU path at run time without a black
    // screen.
    let node = haver_codec::vaapi::render_node(None);
    let gpu = Arc::new(AtomicBool::new(haver_gl::supports_dmabuf_import(&node)));
    tracing::info!(gpu = gpu.load(Ordering::Relaxed), "initial display path");

    let stats = gtk::Label::builder().halign(gtk::Align::Start).valign(gtk::Align::Start).css_classes(["stats"]).visible(false).build();
    let status = gtk::Label::builder().label("Not connected").build();

    let area = gtk::GLArea::builder().hexpand(true).vexpand(true).build();
    let renderer: Rc<RefCell<Option<haver_gl::Renderer>>> = Rc::new(RefCell::new(None));
    let frame: Rc<RefCell<Option<net::DecodedFrame>>> = Rc::new(RefCell::new(None));
    {
        let renderer = renderer.clone();
        let frame = frame.clone();
        let gpu = gpu.clone();
        area.connect_render(move |area, _| {
            render_gl(area, &renderer, &frame, &gpu);
            glib::Propagation::Stop
        });
    }
    {
        // Drop the renderer when the GL context goes away, deleting its objects
        // on the correct (current) context.
        let renderer = renderer.clone();
        area.connect_unrealize(move |area| {
            area.make_current();
            renderer.borrow_mut().take();
        });
    }
    let gl = GlView { area: area.clone(), frame };
    let video: gtk::Widget = area.upcast();

    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&video));
    overlay.add_overlay(&stats);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&header);
    content.append(&overlay);
    content.append(&status);
    window.set_content(Some(&content));

    let ui = Rc::new(App {
        video: video.clone(),
        gl,
        gpu,
        stats: stats.clone(),
        status: status.clone(),
        stream_size: Rc::new(RefCell::new((0, 0))),
        input_tx: Rc::new(RefCell::new(None)),
        endpoint: Rc::new(RefCell::new(None)),
        retries: Rc::new(RefCell::new(0)),
        last_remote_clip: Rc::new(RefCell::new(None)),
    });

    let hotkey = ReleaseHotkey::parse(&cli.release_hotkey).unwrap_or_else(|e| {
        tracing::warn!(error = %e, "invalid --release-hotkey; shortcut release disabled");
        ReleaseHotkey::None
    });
    install_input_handlers(&ui, &video, &window, hotkey);

    // Fullscreen toggle.
    {
        let window = window.clone();
        fullscreen_btn.connect_toggled(move |b| {
            if b.is_active() {
                window.fullscreen();
            } else {
                window.unfullscreen();
            }
        });
    }

    // Connect button.
    {
        let ui = ui.clone();
        let host_entry = host_entry.clone();
        let connect = cli.connect.clone();
        let server_bin = cli.server_bin.clone();
        let headless = cli.headless;
        connect_btn.connect_clicked(move |_| {
            let endpoint = match &connect {
                Some(addr) => Endpoint::Tcp(addr.clone()),
                None => {
                    let host = host_entry.text().to_string();
                    if host.is_empty() {
                        ui.status.set_text("Enter a host first");
                        return;
                    }
                    let mut t = SshTarget::new(host);
                    t.server_bin = server_bin.clone();
                    if headless {
                        t.server_args.push("--headless".into());
                    }
                    Endpoint::Ssh(t)
                }
            };
            *ui.retries.borrow_mut() = 0;
            start_session(ui.clone(), endpoint);
        });
    }

    let css = gtk::CssProvider::new();
    css.load_from_string(".stats { background: rgba(0,0,0,0.6); color: #fff; padding: 6px; margin: 6px; border-radius: 6px; font-family: monospace; }");
    if let Some(display) = gdk::Display::default() {
        gtk::style_context_add_provider_for_display(&display, &css, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
    }

    // Watch the local clipboard: when it changes to text we did not just
    // receive from the remote, forward it to the server.
    if let Some(display) = gdk::Display::default() {
        let clipboard = display.clipboard();
        let ui = ui.clone();
        clipboard.connect_changed(move |cb| {
            let ui = ui.clone();
            cb.read_text_async(gtk::gio::Cancellable::NONE, move |res| {
                if let Ok(Some(text)) = res {
                    let text = text.to_string();
                    // One-shot echo guard: suppress only the value we just set
                    // from the remote, so a later genuine local re-copy is sent.
                    if ui.last_remote_clip.borrow().as_deref() == Some(text.as_str()) {
                        ui.last_remote_clip.borrow_mut().take();
                        return;
                    }
                    if text.len() as u64 > haver_proto::CLIPBOARD_MAX {
                        return;
                    }
                    let bytes = text.into_bytes();
                    let total = bytes.len() as u64;
                    send_payload(
                        &ui,
                        ClientMsg::ClipboardData { mime_type: "text/plain;charset=utf-8".into(), offset: 0, total, data_len: bytes.len() as u32 },
                        bytes,
                    );
                }
            });
        });
    }

    window.present();

    // Auto-connect when an endpoint was given on the command line.
    if cli.connect.is_some() || cli.host.is_some() {
        connect_btn.emit_clicked();
    }
}

fn start_session(ui: Rc<App>, endpoint: Endpoint) {
    let (frame_tx, frame_rx) = sync_channel::<DecodedFrame>(2);
    let (status_tx, status_rx) = channel::<Status>();
    let (input_tx, input_rx) = unbounded_channel::<(ClientMsg, Vec<u8>)>();
    *ui.input_tx.borrow_mut() = Some(input_tx);
    *ui.endpoint.borrow_mut() = Some(endpoint.clone());
    ui.status.set_text("Connecting…");

    let gpu = ui.gpu.clone();
    std::thread::Builder::new()
        .name("haver-net".into())
        .spawn(move || {
            Worker { endpoint, gpu, frames: frame_tx, status: status_tx, input: input_rx }.run();
        })
        .expect("spawn network thread");

    poll_frames(ui.clone(), frame_rx);
    poll_status(ui, status_rx);
}

const MAX_RETRIES: u32 = 5;

/// Schedule a reconnect after a short delay, unless we have exhausted retries.
fn schedule_reconnect(ui: Rc<App>) {
    let n = { let mut r = ui.retries.borrow_mut(); *r += 1; *r };
    if n > MAX_RETRIES {
        ui.status.set_text("Disconnected — press Connect to retry");
        return;
    }
    let Some(endpoint) = ui.endpoint.borrow().clone() else { return };
    ui.status.set_text(&format!("Reconnecting… (attempt {n})"));
    let ui2 = ui.clone();
    glib::timeout_add_local_once(Duration::from_millis(1500), move || {
        start_session(ui2, endpoint);
    });
}

/// Pull decoded frames on the GTK main loop, latest-wins, and paint them.
fn poll_frames(ui: Rc<App>, rx: Receiver<DecodedFrame>) {
    glib::timeout_add_local(Duration::from_millis(8), move || {
        let mut latest = None;
        loop {
            match rx.try_recv() {
                Ok(f) => latest = Some(f),
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                // The worker ended; stop this per-session timer so it does not
                // accumulate across reconnects.
                Err(std::sync::mpsc::TryRecvError::Disconnected) => return glib::ControlFlow::Break,
            }
        }
        if let Some(f) = latest {
            *ui.gl.frame.borrow_mut() = Some(f);
            ui.gl.area.queue_render();
        }
        glib::ControlFlow::Continue
    });
}

/// The Y (R8) and UV (GR88) dmabuf planes of an NV12 frame.
fn nv12_planes(frame: &haver_codec::frame::Nv12Frame) -> (haver_gl::DmabufPlane<'_>, haver_gl::DmabufPlane<'_>) {
    use drm_fourcc::DrmFourcc;
    use haver_gl::DmabufPlane;
    let info = frame.info();
    (
        DmabufPlane { fd: info.fd(), width: info.width, height: info.height, offset: info.planes[0].offset, stride: info.planes[0].stride, fourcc: DrmFourcc::R8, modifier: info.modifier },
        DmabufPlane { fd: info.fd(), width: info.width / 2, height: info.height / 2, offset: info.planes[1].offset, stride: info.planes[1].stride, fourcc: DrmFourcc::Gr88, modifier: info.modifier },
    )
}

/// Render the latest frame in the GLArea's current context. Draws dmabuf planes
/// on the GPU when possible; if that is unavailable or fails, clears the shared
/// `gpu` flag (so the worker switches to sending BGRA) and blits CPU BGRA.
fn render_gl(area: &gtk::GLArea, renderer: &Rc<RefCell<Option<haver_gl::Renderer>>>, frame: &Rc<RefCell<Option<net::DecodedFrame>>>, gpu: &Arc<AtomicBool>) {
    use haver_gl::FramePlanes;

    if renderer.borrow().is_none() {
        match haver_gl::Renderer::new() {
            Ok(r) => {
                if !r.can_dmabuf() {
                    // The real context cannot import dmabufs; fall back so the
                    // worker sends BGRA instead of planes.
                    gpu.store(false, Ordering::Relaxed);
                }
                *renderer.borrow_mut() = Some(r);
            }
            Err(e) => {
                tracing::error!(error = %e, "GL renderer init failed");
                return;
            }
        }
    }
    let renderer = renderer.borrow();
    let Some(renderer) = renderer.as_ref() else { return };
    let frame = frame.borrow();
    let Some(frame) = frame.as_ref() else { return };

    let scale = area.scale_factor();
    let (fb_w, fb_h) = (area.width() * scale, area.height() * scale);
    match frame {
        net::DecodedFrame::Planes { width, height, main, aux } => {
            let (main_y, main_uv) = nv12_planes(main);
            let planes = FramePlanes { main_y, main_uv, aux: aux.as_ref().map(|a| nv12_planes(a)), width: *width as u32, height: *height as u32 };
            if let Err(e) = renderer.draw_planes(&planes, fb_w, fb_h) {
                tracing::warn!(error = %e, "GL plane draw failed; falling back to CPU");
                gpu.store(false, Ordering::Relaxed);
            }
        }
        net::DecodedFrame::Rgba { width, height, bgra } => {
            if let Err(e) = renderer.draw_rgba(bgra, *width as i32, *height as i32, fb_w, fb_h) {
                tracing::warn!(error = %e, "GL blit failed");
            }
        }
    }
}

fn poll_status(ui: Rc<App>, rx: Receiver<Status>) {
    glib::timeout_add_local(Duration::from_millis(100), move || {
        while let Ok(s) = rx.try_recv() {
            match s {
                Status::Connected { width, height } => {
                    *ui.stream_size.borrow_mut() = (width, height);
                    *ui.retries.borrow_mut() = 0;
                    ui.status.set_text(&format!("Connected — {width}x{height}"));
                }
                Status::Stats { fps, mbit, decode_ms } => {
                    ui.stats.set_visible(true);
                    ui.stats.set_text(&format!("{fps:.0} fps  {mbit:.1} Mbit/s  decode {decode_ms:.1} ms"));
                }
                Status::Cursor { width, height, hot_x, hot_y, argb } => set_remote_cursor(&ui, width, height, hot_x, hot_y, &argb),
                Status::Clipboard(text) => {
                    *ui.last_remote_clip.borrow_mut() = Some(text.clone());
                    if let Some(display) = gdk::Display::default() {
                        display.clipboard().set_text(&text);
                    }
                }
                Status::Error(e) => {
                    ui.status.set_text(&format!("Error: {e}"));
                    schedule_reconnect(ui.clone());
                    return glib::ControlFlow::Break;
                }
                Status::Closed => {
                    schedule_reconnect(ui.clone());
                    return glib::ControlFlow::Break;
                }
            }
        }
        glib::ControlFlow::Continue
    });
}

/// Show the remote cursor as the video widget's own cursor, so the local
/// compositor draws it at the real pointer position with no added latency.
fn set_remote_cursor(ui: &App, width: u32, height: u32, hot_x: i32, hot_y: i32, argb: &[u8]) {
    if width == 0 || height == 0 || argb.len() < (width * height * 4) as usize {
        return;
    }
    let opaque = argb.chunks_exact(4).any(|p| p[3] != 0);
    if !opaque {
        // Fully transparent: hide the pointer over the video.
        if let Some(cursor) = gdk::Cursor::from_name("none", None) {
            ui.video.set_cursor(Some(&cursor));
        }
        return;
    }
    let bytes = glib::Bytes::from(argb);
    let texture = gdk::MemoryTexture::new(width as i32, height as i32, gdk::MemoryFormat::B8g8r8a8, &bytes, width as usize * 4);
    let cursor = gdk::Cursor::from_texture(&texture, hot_x, hot_y, None);
    ui.video.set_cursor(Some(&cursor));
}

/// Map a widget-space point to remote output coordinates.
fn to_remote(ui: &App, x: f64, y: f64) -> (f64, f64) {
    let (rw, rh) = *ui.stream_size.borrow();
    if rw == 0 || rh == 0 {
        return (0.0, 0.0);
    }
    let (aw, ah) = (ui.video.width() as f64, ui.video.height() as f64);
    // content_fit=Contain: the video is letterboxed; compute the fitted rect.
    let scale = (aw / rw as f64).min(ah / rh as f64);
    let (fw, fh) = (rw as f64 * scale, rh as f64 * scale);
    let (ox, oy) = ((aw - fw) / 2.0, (ah - fh) / 2.0);
    let rx = ((x - ox) / scale).clamp(0.0, rw as f64);
    let ry = ((y - oy) / scale).clamp(0.0, rh as f64);
    (rx, ry)
}

fn send(ui: &App, msg: ClientMsg) {
    if let Some(tx) = ui.input_tx.borrow().as_ref() {
        let _ = tx.send((msg, Vec::new()));
    }
}

/// Send a message with a trailing payload (used for clipboard text).
fn send_payload(ui: &App, msg: ClientMsg, payload: Vec<u8>) {
    if let Some(tx) = ui.input_tx.borrow().as_ref() {
        let _ = tx.send((msg, payload));
    }
}

/// How the user releases captured shortcuts back to the local compositor.
#[derive(Clone)]
enum ReleaseHotkey {
    None,
    DoubleTap { keyval: gdk::Key, within: Duration },
    Chord { mods: gdk::ModifierType, keyval: gdk::Key },
}

const CHORD_MODS: gdk::ModifierType = gdk::ModifierType::CONTROL_MASK
    .union(gdk::ModifierType::ALT_MASK)
    .union(gdk::ModifierType::SHIFT_MASK)
    .union(gdk::ModifierType::SUPER_MASK);

impl ReleaseHotkey {
    fn parse(spec: &str) -> Result<Self, String> {
        let s = spec.trim();
        if s.is_empty() || s.eq_ignore_ascii_case("none") {
            return Ok(Self::None);
        }
        let lower = s.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("double-").or_else(|| lower.strip_prefix("double:")) {
            return Ok(Self::DoubleTap { keyval: key_from_name(rest)?, within: Duration::from_millis(400) });
        }
        let mut mods = gdk::ModifierType::empty();
        let mut keyval = None;
        for tok in s.split('+') {
            match tok.trim().to_ascii_lowercase().as_str() {
                "" => {}
                "ctrl" | "control" => mods |= gdk::ModifierType::CONTROL_MASK,
                "alt" => mods |= gdk::ModifierType::ALT_MASK,
                "shift" => mods |= gdk::ModifierType::SHIFT_MASK,
                "super" | "logo" | "win" | "meta" => mods |= gdk::ModifierType::SUPER_MASK,
                other => keyval = Some(key_from_name(other)?),
            }
        }
        match keyval {
            Some(keyval) => Ok(Self::Chord { mods, keyval }),
            None => Err(format!("no key in release hotkey '{spec}'")),
        }
    }

    /// True if this press is the release trigger. For a double-tap it records
    /// the tap time and returns true only on the quick second press, so the
    /// first tap still reaches the remote.
    fn matches(&self, keyval: gdk::Key, state: gdk::ModifierType, last_tap: &RefCell<Option<Instant>>) -> bool {
        match self {
            Self::None => false,
            Self::Chord { mods, keyval: k } => keyval == *k && (state & CHORD_MODS) == *mods,
            Self::DoubleTap { keyval: k, within } => {
                if keyval != *k {
                    return false;
                }
                let now = Instant::now();
                let mut lt = last_tap.borrow_mut();
                match *lt {
                    Some(prev) if now.duration_since(prev) <= *within => {
                        *lt = None;
                        true
                    }
                    _ => {
                        *lt = Some(now);
                        false
                    }
                }
            }
        }
    }

    fn describe(&self) -> String {
        let name = |k: &gdk::Key| {
            let n = k.name().map(|s| s.to_string()).unwrap_or_else(|| "?".into());
            if n == "Escape" { "Esc".into() } else { n }
        };
        match self {
            Self::None => "release disabled".into(),
            Self::DoubleTap { keyval, .. } => format!("double-tap {}", name(keyval)),
            Self::Chord { mods, keyval } => {
                let mut parts = Vec::new();
                if mods.contains(gdk::ModifierType::CONTROL_MASK) { parts.push("Ctrl".to_string()); }
                if mods.contains(gdk::ModifierType::ALT_MASK) { parts.push("Alt".to_string()); }
                if mods.contains(gdk::ModifierType::SHIFT_MASK) { parts.push("Shift".to_string()); }
                if mods.contains(gdk::ModifierType::SUPER_MASK) { parts.push("Super".to_string()); }
                parts.push(name(keyval));
                parts.join("+")
            }
        }
    }
}

/// Resolve a key name to a `gdk::Key`, accepting lower-case and a few aliases.
fn key_from_name(name: &str) -> Result<gdk::Key, String> {
    let n = name.trim();
    let alias = match n.to_ascii_lowercase().as_str() {
        "esc" => Some("Escape"),
        "enter" | "return" => Some("Return"),
        "space" => Some("space"),
        _ => None,
    };
    let title = {
        let mut c = n.chars();
        c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
    };
    for cand in alias.iter().copied().chain([n, title.as_str()]) {
        if !cand.is_empty() {
            if let Some(k) = gdk::Key::from_name(cand) {
                return Ok(k);
            }
        }
    }
    Err(format!("unknown key '{name}'"))
}

/// Give the keyboard back to the local compositor by dropping video focus,
/// which fires the focus-leave handler that restores system shortcuts.
fn release_capture(ui: &App, window: &adw::ApplicationWindow) {
    gtk::prelude::GtkWindowExt::set_focus(window, gtk::Widget::NONE);
    ui.status.set_text("Shortcuts released — click the screen to capture again");
}

fn install_input_handlers(ui: &Rc<App>, video: &gtk::Widget, window: &adw::ApplicationWindow, hotkey: ReleaseHotkey) {
    video.set_focusable(true);
    video.set_can_focus(true);

    // Keyboard: hardware keycode minus 8 is the evdev code.
    let key = gtk::EventControllerKey::new();
    let last_tap: Rc<RefCell<Option<Instant>>> = Rc::new(RefCell::new(None));
    {
        let ui = ui.clone();
        let window = window.clone();
        let hotkey = hotkey.clone();
        let last_tap = last_tap.clone();
        key.connect_key_pressed(move |_, keyval, keycode, state| {
            if hotkey.matches(keyval, state, &last_tap) {
                release_capture(&ui, &window);
                return glib::Propagation::Stop;
            }
            send(&ui, ClientMsg::Key { keycode: keycode.saturating_sub(8), pressed: true });
            glib::Propagation::Stop
        });
    }
    {
        let ui = ui.clone();
        key.connect_key_released(move |_, _keyval, keycode, _state| {
            send(&ui, ClientMsg::Key { keycode: keycode.saturating_sub(8), pressed: false });
        });
    }
    video.add_controller(key);

    // Pointer motion.
    let motion = gtk::EventControllerMotion::new();
    {
        let ui = ui.clone();
        motion.connect_motion(move |_, x, y| {
            let (rx, ry) = to_remote(&ui, x, y);
            send(&ui, ClientMsg::PointerMotion { x: rx, y: ry });
        });
    }
    video.add_controller(motion);

    // Buttons.
    let click = gtk::GestureClick::new();
    click.set_button(0); // any button
    {
        let ui = ui.clone();
        let video = video.clone();
        click.connect_pressed(move |g, _, _, _| {
            video.grab_focus();
            send(&ui, ClientMsg::PointerButton { button: evdev_button(g.current_button()), pressed: true });
        });
    }
    {
        let ui = ui.clone();
        click.connect_released(move |g, _, _, _| {
            send(&ui, ClientMsg::PointerButton { button: evdev_button(g.current_button()), pressed: false });
        });
    }
    video.add_controller(click);

    // Scroll.
    let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
    {
        let ui = ui.clone();
        scroll.connect_scroll(move |_, dx, dy| {
            if dy != 0.0 {
                send(&ui, ClientMsg::PointerAxis { axis: Axis::Vertical, value: dy * 15.0, discrete: Some(dy.signum() as i32), stop: false });
            }
            if dx != 0.0 {
                send(&ui, ClientMsg::PointerAxis { axis: Axis::Horizontal, value: dx * 15.0, discrete: Some(dx.signum() as i32), stop: false });
            }
            glib::Propagation::Stop
        });
    }
    video.add_controller(scroll);

    // Debounced resize: GTK4 Picture has no resize signal, so poll the widget
    // allocation and, once it has been stable for ~200 ms, tell the server.
    {
        let ui = ui.clone();
        let video = video.clone();
        let last_sent = Rc::new(RefCell::new((0i32, 0i32)));
        let stable = Rc::new(RefCell::new((0i32, 0i32, 0u32)));
        glib::timeout_add_local(Duration::from_millis(100), move || {
            let scale = video.scale_factor();
            let (w, h) = (video.width() * scale, video.height() * scale);
            if w < 64 || h < 64 {
                return glib::ControlFlow::Continue;
            }
            let mut st = stable.borrow_mut();
            if (st.0, st.1) == (w, h) {
                st.2 += 1;
            } else {
                *st = (w, h, 0);
            }
            if st.2 == 2 && (last_sent.borrow().0, last_sent.borrow().1) != (w, h) {
                *last_sent.borrow_mut() = (w, h);
                send(&ui, ClientMsg::Resize { width: w as u32, height: h as u32, scale: scale as f32 });
            }
            glib::ControlFlow::Continue
        });
    }

    // While the video has focus, route system shortcuts (Super, Alt-Tab, ...)
    // to the remote session instead of the local compositor.
    let focus = gtk::EventControllerFocus::new();
    {
        let window = window.clone();
        let ui = ui.clone();
        let hint = match &hotkey {
            ReleaseHotkey::None => None,
            hk => Some(format!("Shortcuts captured — {} to release", hk.describe())),
        };
        focus.connect_enter(move |_| {
            if let Some(toplevel) = window.surface().and_downcast::<gdk::Toplevel>() {
                toplevel.inhibit_system_shortcuts(None::<&gdk::Event>);
            }
            if let Some(hint) = &hint {
                ui.status.set_text(hint);
            }
        });
    }
    {
        let window = window.clone();
        focus.connect_leave(move |_| {
            if let Some(toplevel) = window.surface().and_downcast::<gdk::Toplevel>() {
                toplevel.restore_system_shortcuts();
            }
        });
    }
    video.add_controller(focus);
}

/// GTK button number to evdev `BTN_*`.
fn evdev_button(n: u32) -> u32 {
    match n {
        1 => 0x110, // BTN_LEFT
        2 => 0x112, // BTN_MIDDLE
        3 => 0x111, // BTN_RIGHT
        8 => 0x116, // BTN_SIDE (back)
        9 => 0x115, // BTN_EXTRA (forward)
        _ => 0x110,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn parse_chord_and_double_and_none() {
        assert!(matches!(ReleaseHotkey::parse("none").unwrap(), ReleaseHotkey::None));
        assert!(matches!(ReleaseHotkey::parse("").unwrap(), ReleaseHotkey::None));

        match ReleaseHotkey::parse("shift+escape").unwrap() {
            ReleaseHotkey::Chord { mods, keyval } => {
                assert_eq!(mods, gdk::ModifierType::SHIFT_MASK);
                assert_eq!(keyval, gdk::Key::Escape);
            }
            _ => panic!("expected chord"),
        }

        match ReleaseHotkey::parse("Ctrl+Alt+q").unwrap() {
            ReleaseHotkey::Chord { mods, .. } => {
                assert!(mods.contains(gdk::ModifierType::CONTROL_MASK));
                assert!(mods.contains(gdk::ModifierType::ALT_MASK));
            }
            _ => panic!("expected chord"),
        }

        assert!(matches!(ReleaseHotkey::parse("double-escape").unwrap(), ReleaseHotkey::DoubleTap { .. }));
        assert!(ReleaseHotkey::parse("ctrl+alt").is_err());
    }

    #[test]
    fn chord_matches_only_with_exact_mods() {
        let hk = ReleaseHotkey::parse("shift+escape").unwrap();
        let lt = RefCell::new(None);
        assert!(hk.matches(gdk::Key::Escape, gdk::ModifierType::SHIFT_MASK, &lt));
        // Bare Escape, no Shift: not a match (so it reaches the remote).
        assert!(!hk.matches(gdk::Key::Escape, gdk::ModifierType::empty(), &lt));
        // Extra lock bits are ignored.
        assert!(hk.matches(gdk::Key::Escape, gdk::ModifierType::SHIFT_MASK | gdk::ModifierType::LOCK_MASK, &lt));
    }

    #[test]
    fn double_tap_needs_two_quick_presses() {
        let hk = ReleaseHotkey::parse("double-escape").unwrap();
        let lt = RefCell::new(None);
        let none = gdk::ModifierType::empty();
        assert!(!hk.matches(gdk::Key::Escape, none, &lt)); // first tap forwarded
        assert!(hk.matches(gdk::Key::Escape, none, &lt)); // quick second releases
        assert!(!hk.matches(gdk::Key::Escape, none, &lt)); // counter reset
    }
}
