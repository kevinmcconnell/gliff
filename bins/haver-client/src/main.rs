//! haver-client: a GTK4/libadwaita window that connects to a haver server,
//! decodes the video, shows it, and forwards keyboard and pointer input.
//!
//! Decode runs on a worker thread (see `net`); this file is the UI. There is no
//! `unsafe` here: decoded frames arrive as BGRA and become a `gdk::MemoryTexture`.

mod net;

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc::{channel, sync_channel, Receiver};
use std::time::Duration;

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
}

/// Everything the UI shares with its callbacks.
struct App {
    picture: gtk::Picture,
    stats: gtk::Label,
    status: gtk::Label,
    stream_size: Rc<RefCell<(u32, u32)>>,
    input_tx: Rc<RefCell<Option<UnboundedSender<ClientMsg>>>>,
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

    let picture = gtk::Picture::builder().hexpand(true).vexpand(true).content_fit(gtk::ContentFit::Contain).build();
    let stats = gtk::Label::builder().halign(gtk::Align::Start).valign(gtk::Align::Start).css_classes(["stats"]).visible(false).build();
    let status = gtk::Label::builder().label("Not connected").build();

    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&picture));
    overlay.add_overlay(&stats);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&header);
    content.append(&overlay);
    content.append(&status);
    window.set_content(Some(&content));

    let ui = Rc::new(App {
        picture: picture.clone(),
        stats: stats.clone(),
        status: status.clone(),
        stream_size: Rc::new(RefCell::new((0, 0))),
        input_tx: Rc::new(RefCell::new(None)),
    });

    install_input_handlers(&ui, &picture, &window);

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
            start_session(ui.clone(), endpoint);
        });
    }

    let css = gtk::CssProvider::new();
    css.load_from_string(".stats { background: rgba(0,0,0,0.6); color: #fff; padding: 6px; margin: 6px; border-radius: 6px; font-family: monospace; }");
    if let Some(display) = gdk::Display::default() {
        gtk::style_context_add_provider_for_display(&display, &css, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
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
    let (input_tx, input_rx) = unbounded_channel::<ClientMsg>();
    *ui.input_tx.borrow_mut() = Some(input_tx);
    ui.status.set_text("Connecting…");

    std::thread::Builder::new()
        .name("haver-net".into())
        .spawn(move || {
            Worker { endpoint, frames: frame_tx, status: status_tx, input: input_rx }.run();
        })
        .expect("spawn network thread");

    poll_frames(ui.clone(), frame_rx);
    poll_status(ui, status_rx);
}

/// Pull decoded frames on the GTK main loop, latest-wins, and paint them.
fn poll_frames(ui: Rc<App>, rx: Receiver<DecodedFrame>) {
    glib::timeout_add_local(Duration::from_millis(8), move || {
        let mut latest = None;
        while let Ok(f) = rx.try_recv() {
            latest = Some(f);
        }
        if let Some(f) = latest {
            let bytes = glib::Bytes::from(&f.bgra);
            let texture = gdk::MemoryTexture::new(f.width as i32, f.height as i32, gdk::MemoryFormat::B8g8r8a8, &bytes, f.width * 4);
            ui.picture.set_paintable(Some(&texture));
        }
        glib::ControlFlow::Continue
    });
}

fn poll_status(ui: Rc<App>, rx: Receiver<Status>) {
    glib::timeout_add_local(Duration::from_millis(100), move || {
        while let Ok(s) = rx.try_recv() {
            match s {
                Status::Connected { width, height } => {
                    *ui.stream_size.borrow_mut() = (width, height);
                    ui.status.set_text(&format!("Connected — {width}x{height}"));
                }
                Status::Stats { fps, mbit, decode_ms } => {
                    ui.stats.set_visible(true);
                    ui.stats.set_text(&format!("{fps:.0} fps  {mbit:.1} Mbit/s  decode {decode_ms:.1} ms"));
                }
                Status::Error(e) => ui.status.set_text(&format!("Error: {e}")),
                Status::Closed => ui.status.set_text("Disconnected"),
            }
        }
        glib::ControlFlow::Continue
    });
}

/// Map a widget-space point to remote output coordinates.
fn to_remote(ui: &App, x: f64, y: f64) -> (f64, f64) {
    let (rw, rh) = *ui.stream_size.borrow();
    if rw == 0 || rh == 0 {
        return (0.0, 0.0);
    }
    let (aw, ah) = (ui.picture.width() as f64, ui.picture.height() as f64);
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
        let _ = tx.send(msg);
    }
}

fn install_input_handlers(ui: &Rc<App>, picture: &gtk::Picture, window: &adw::ApplicationWindow) {
    picture.set_focusable(true);
    picture.set_can_focus(true);

    // Keyboard: hardware keycode minus 8 is the evdev code.
    let key = gtk::EventControllerKey::new();
    {
        let ui = ui.clone();
        key.connect_key_pressed(move |_, _keyval, keycode, _state| {
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
    picture.add_controller(key);

    // Pointer motion.
    let motion = gtk::EventControllerMotion::new();
    {
        let ui = ui.clone();
        motion.connect_motion(move |_, x, y| {
            let (rx, ry) = to_remote(&ui, x, y);
            send(&ui, ClientMsg::PointerMotion { x: rx, y: ry });
        });
    }
    picture.add_controller(motion);

    // Buttons.
    let click = gtk::GestureClick::new();
    click.set_button(0); // any button
    {
        let ui = ui.clone();
        let picture = picture.clone();
        click.connect_pressed(move |g, _, _, _| {
            picture.grab_focus();
            send(&ui, ClientMsg::PointerButton { button: evdev_button(g.current_button()), pressed: true });
        });
    }
    {
        let ui = ui.clone();
        click.connect_released(move |g, _, _, _| {
            send(&ui, ClientMsg::PointerButton { button: evdev_button(g.current_button()), pressed: false });
        });
    }
    picture.add_controller(click);

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
    picture.add_controller(scroll);

    // Debounced resize: GTK4 Picture has no resize signal, so poll the widget
    // allocation and, once it has been stable for ~200 ms, tell the server.
    {
        let ui = ui.clone();
        let picture = picture.clone();
        let last_sent = Rc::new(RefCell::new((0i32, 0i32)));
        let stable = Rc::new(RefCell::new((0i32, 0i32, 0u32)));
        glib::timeout_add_local(Duration::from_millis(100), move || {
            let scale = picture.scale_factor();
            let (w, h) = (picture.width() * scale, picture.height() * scale);
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

    let _ = window;
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
