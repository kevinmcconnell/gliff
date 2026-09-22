//! gliff: a GTK4/libadwaita window that connects to a gliff server,
//! decodes the video, shows it, and forwards keyboard and pointer input.
//!
//! Decode runs on a worker thread (see `net`); this file is the UI. Decoded
//! frames arrive as dmabufs, which GTK imports as textures, so the UI has no
//! GPU code of its own; its one `unsafe` block hands GTK a dmabuf fd.

mod clipboard;
mod clipboard_ui;
mod keymap;
mod net;
mod paintable;
mod recent;
mod theme;

use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc::{channel, sync_channel, Receiver};
use std::time::{Duration, Instant};

use adw::prelude::*;
use clap::Parser;
use gliff_proto::{Axis, ClientMsg};
use gliff_transport::SshTarget;
use gliff_vk::DisplayFrame;
use gtk::gdk;
use gtk::glib;
use gtk4 as gtk;
use libadwaita as adw;
use net::Frame;
use net::{Endpoint, Status, Worker};
use recent::Config;
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

/// A message plus optional trailing payload, sent from the UI to the worker.
type OutSender = UnboundedSender<clipboard::ToWorker>;

#[derive(Parser, Clone)]
#[command(name = "gliff", about = "Remote-desktop a Hyprland session over ssh")]
struct Cli {
    /// `user@host` to ssh to and spawn gliff-server. Without it the window
    /// opens with the address bar focused, offering the machines used most
    /// recently.
    host: Option<String>,
    /// Dev: connect directly to a `gliff-server --listen` address.
    #[arg(long)]
    connect: Option<String>,
    /// Remote gliff-server path.
    #[arg(long, default_value = "gliff-server")]
    server_bin: String,
    /// Create a private headless output on the remote, sized and scaled to
    /// this window, instead of mirroring the remote's focused screen.
    #[arg(long, conflicts_with = "output")]
    headless: bool,
    /// Mirror the named remote output (e.g. `DP-1`) instead of the focused one.
    #[arg(long)]
    output: Option<String>,
    /// Hotkey that releases captured shortcuts and hands the keyboard back to
    /// the local compositor. Forms: a chord like `shift+escape`, `ctrl+alt+q`
    /// or `super+escape`; `double-<key>` for a double-tap (e.g.
    /// `double-escape`); or `none` to disable. The screen recaptures when you
    /// click it again.
    #[arg(long, default_value = "shift+escape")]
    release_hotkey: String,
    /// Video pipeline: `gpu` (Vulkan Video, falling back to the CPU when
    /// unavailable) or `cpu` (force OpenH264 on the CPU). Overrides the
    /// GLIFF_VIDEO environment variable.
    #[arg(long, value_parser = ["gpu", "cpu"])]
    video: Option<String>,
}

/// Everything the UI shares with its callbacks.
struct App {
    window: adw::ApplicationWindow,
    /// The picture, upcast; input controllers attach to it and we measure it.
    video: gtk::Widget,
    /// What the picture shows: the latest frame at 1:1 device pixels.
    frame: paintable::FramePaintable,
    stats: gtk::Label,
    status: gtk::Label,
    /// The address bar: shows the current machine; Enter connects to what
    /// was typed instead.
    entry: gtk::Entry,
    /// Drops below the address bar with the recent machines while it has
    /// focus.
    recent_popover: gtk::Popover,
    recent_list: gtk::ListBox,
    transfers: Rc<clipboard_ui::TransferBars>,
    /// Size of the stream the server is sending, from the last StreamConfig.
    stream_size: Cell<(u32, u32)>,
    /// The remote output's scale: pointer coordinates go in physical / scale.
    stream_scale: Cell<f32>,
    /// Size of the video widget in device pixels, rounded down to even.
    view_size: Cell<(u32, u32)>,
    /// The size last asked of the server, so a pending resize is not repeated.
    resize_requested: Cell<(u32, u32)>,
    input_tx: RefCell<Option<OutSender>>,
    /// Evdev codes currently held on the remote, so they can all be released
    /// when the keyboard is handed back to the local compositor.
    pressed_keys: RefCell<BTreeSet<u32>>,
    /// The last endpoint, kept so a dropped connection can be retried.
    endpoint: RefCell<Option<Endpoint>>,
    /// Consecutive failed connection attempts, reset on a successful connect.
    retries: Cell<u32>,
    /// Bumped by every `start_session`, so pollers and delayed reconnects of
    /// an earlier session can tell they are stale.
    session: Cell<u64>,
    /// Connects to the address bar's machine; turns into a Reconnect button
    /// once automatic reconnects have given up.
    connect_btn: gtk::Button,
    /// The `user@host` in the address bar, recorded in the recent list on
    /// the first successful connect; `None` for a dev `--connect` session.
    machine: RefCell<Option<String>>,
    remembered: Cell<bool>,
    config_path: PathBuf,
    cli: Cli,
}

fn main() -> glib::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let cli = Cli::parse();
    let app = adw::Application::builder()
        .application_id("com.gliff.Client")
        .build();
    // The theme is per display, so it is set up once. The monitor lives in
    // this closure for the app's lifetime; dropping it would stop theme
    // updates.
    let theme_monitor = RefCell::new(None);
    app.connect_startup(move |_| {
        *theme_monitor.borrow_mut() = theme::follow_omarchy_theme();
    });
    app.connect_activate(move |app| build_ui(app, &cli));
    // GTK owns argv parsing; we already parsed with clap, so pass none.
    let empty: Vec<String> = vec![];
    app.run_with_args(&empty)
}

/// The endpoint named on the command line, if any.
fn endpoint_from_cli(cli: &Cli) -> Option<Endpoint> {
    if let Some(addr) = &cli.connect {
        return Some(Endpoint::Tcp(addr.clone()));
    }
    cli.host.as_deref().map(|host| ssh_endpoint(cli, host))
}

fn ssh_endpoint(cli: &Cli, host: &str) -> Endpoint {
    let mut t = SshTarget::new(host.to_string());
    t.server_bin = cli.server_bin.clone();
    t.server_args = match (&cli.output, cli.headless) {
        (Some(name), _) => vec!["--output".into(), name.clone()],
        (None, true) => vec!["--headless".into()],
        (None, false) => vec!["--output".into(), "auto".into()],
    };
    Endpoint::Ssh(t)
}

/// What the address bar shows for an endpoint.
fn machine_name(endpoint: &Endpoint) -> &str {
    match endpoint {
        Endpoint::Ssh(t) => t.host.as_str(),
        Endpoint::Tcp(addr) => addr.as_str(),
    }
}

/// Title in the `Document — App` form used by GNOME apps, so the machine
/// is what a task switcher shows first.
fn window_title(endpoint: &Endpoint) -> String {
    format!("{} — Gliff", machine_name(endpoint))
}

/// The one window: a header with the machine address bar, the remote
/// screen (black until connected), and a status line.
fn build_ui(app: &adw::Application, cli: &Cli) {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Gliff")
        .default_width(1280)
        .default_height(760)
        .build();

    let header = adw::HeaderBar::builder().show_title(false).build();
    let entry = gtk::Entry::builder()
        .placeholder_text("user@host")
        .width_chars(32)
        .build();
    let connect_btn = gtk::Button::builder()
        .icon_name(CONNECT_ICON)
        .tooltip_text("Connect")
        .build();
    let address_bar = gtk::Box::builder().css_classes(["linked"]).build();
    address_bar.append(&entry);
    address_bar.append(&connect_btn);
    let fullscreen_btn = gtk::ToggleButton::builder()
        .icon_name("view-fullscreen-symbolic")
        .build();
    let stats_btn = gtk::ToggleButton::builder()
        .icon_name("utilities-system-monitor-symbolic")
        .tooltip_text("Show stats")
        .build();
    header.pack_start(&address_bar);
    header.pack_end(&fullscreen_btn);
    header.pack_end(&stats_btn);

    // The recent machines drop below the address bar while it has focus.
    let recent_list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    let recent_popover = gtk::Popover::builder()
        .child(&recent_list)
        .autohide(false)
        .has_arrow(false)
        .position(gtk::PositionType::Bottom)
        .build();
    recent_popover.set_parent(&entry);

    let stats = gtk::Label::builder()
        .halign(gtk::Align::Start)
        .valign(gtk::Align::Start)
        .css_classes(["stats"])
        .build();
    stats_btn
        .bind_property("active", &stats, "visible")
        .sync_create()
        .build();
    let status = gtk::Label::builder().label("Not connected").build();

    // The video is a plain Picture: each decoded frame is a dmabuf that GTK
    // imports as a texture and letterboxes with Contain.
    let picture = gtk::Picture::builder()
        .hexpand(true)
        .vexpand(true)
        .can_shrink(true)
        .content_fit(gtk::ContentFit::ScaleDown)
        .css_classes(["video"])
        .build();
    let frame = paintable::FramePaintable::default();
    picture.set_paintable(Some(&frame));
    let video: gtk::Widget = picture.clone().upcast();

    let transfers = Rc::new(clipboard_ui::TransferBars::new());
    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&video));
    overlay.add_overlay(&stats);
    overlay.add_overlay(transfers.widget());

    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&header);
    content.append(&overlay);
    content.append(&status);
    window.set_content(Some(&content));

    let ui = Rc::new(App {
        window: window.clone(),
        video: video.clone(),
        frame,
        stats: stats.clone(),
        status: status.clone(),
        entry: entry.clone(),
        recent_popover: recent_popover.clone(),
        recent_list: recent_list.clone(),
        transfers,
        stream_size: Cell::new((0, 0)),
        stream_scale: Cell::new(1.0),
        view_size: Cell::new((0, 0)),
        resize_requested: Cell::new((0, 0)),
        input_tx: RefCell::new(None),
        pressed_keys: RefCell::new(BTreeSet::new()),
        endpoint: RefCell::new(None),
        retries: Cell::new(0),
        session: Cell::new(0),
        connect_btn: connect_btn.clone(),
        machine: RefCell::new(None),
        remembered: Cell::new(false),
        config_path: Config::default_path(),
        cli: cli.clone(),
    });

    let hotkey = ReleaseHotkey::parse(&cli.release_hotkey).unwrap_or_else(|e| {
        tracing::warn!(error = %e, "invalid --release-hotkey; shortcut release disabled");
        ReleaseHotkey::None
    });
    install_input_handlers(&ui, &video, &window, hotkey);
    install_resize_handler(&ui);
    install_address_bar(&ui);

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

    let css = gtk::CssProvider::new();
    css.load_from_string(concat!(
        ".video { background: #000; }",
        ".stats { background: rgba(0,0,0,0.6); color: #fff; padding: 6px; margin: 6px; border-radius: 6px; font-family: monospace; }",
        ".transfers { margin: 6px; }",
        ".transfer { background: rgba(0,0,0,0.7); color: #fff; padding: 6px 8px; border-radius: 6px; }",
    ));
    if let Some(display) = gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &css,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }

    // Offer every local clipboard change to the server; our own proxy for
    // the server's selection does not count as a change.
    {
        let ui = ui.clone();
        clipboard_ui::watch_local(move || ui.input_tx.borrow().clone());
    }

    window.present();
    match endpoint_from_cli(cli) {
        Some(endpoint) => connect_to(&ui, endpoint),
        None => {
            entry.grab_focus();
        }
    }
}

const CONNECT_ICON: &str = "go-next-symbolic";
const RECONNECT_ICON: &str = "view-refresh-symbolic";

/// Wire the address bar: Enter connects to what was typed, Escape gives up
/// the edit, and the recent machines drop down while it has focus.
fn install_address_bar(ui: &Rc<App>) {
    {
        let ui = ui.clone();
        ui.entry
            .clone()
            .connect_activate(move |_| connect_from_address_bar(&ui));
    }
    {
        let ui = ui.clone();
        ui.connect_btn
            .clone()
            .connect_clicked(move |_| connect_from_address_bar(&ui));
    }
    // The window may become active with the address bar already focused,
    // as a bare `gliff` does, so the drop-down follows activation too.
    {
        let ui = ui.clone();
        ui.window.clone().connect_is_active_notify(move |w| {
            if !w.is_active() {
                ui.recent_popover.popdown();
                return;
            }
            let ui = ui.clone();
            glib::idle_add_local_once(move || {
                let in_entry = gtk::prelude::GtkWindowExt::focus(&ui.window)
                    .is_some_and(|f| f.is_ancestor(&ui.entry));
                if in_entry {
                    show_recents(&ui);
                }
            });
        });
    }
    {
        let ui = ui.clone();
        ui.recent_list.clone().connect_row_activated(move |_, row| {
            if let Some(row) = row.downcast_ref::<adw::ActionRow>() {
                connect_to(&ui, ssh_endpoint(&ui.cli, &row.title()));
            }
        });
    }

    let focus = gtk::EventControllerFocus::new();
    {
        let ui = ui.clone();
        focus.connect_enter(move |_| show_recents(&ui));
    }
    {
        let ui = ui.clone();
        // Focus may be moving into the drop-down itself (a click on a row, or
        // the Down key), so decide once the new focus is known.
        focus.connect_leave(move |_| {
            let ui = ui.clone();
            glib::idle_add_local_once(move || {
                let in_popover = gtk::prelude::GtkWindowExt::focus(&ui.window)
                    .is_some_and(|w| w.is_ancestor(&ui.recent_popover));
                if !in_popover {
                    ui.recent_popover.popdown();
                }
            });
        });
    }
    ui.entry.add_controller(focus);

    let keys = gtk::EventControllerKey::new();
    {
        let ui = ui.clone();
        keys.connect_key_pressed(move |_, keyval, _, _| match keyval {
            gdk::Key::Escape => {
                cancel_address_edit(&ui);
                glib::Propagation::Stop
            }
            gdk::Key::Down if ui.recent_popover.is_visible() => {
                ui.recent_list.child_focus(gtk::DirectionType::Down);
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
        });
    }
    ui.entry.add_controller(keys);

    let list_keys = gtk::EventControllerKey::new();
    {
        let ui = ui.clone();
        list_keys.connect_key_pressed(move |_, keyval, _, _| {
            if keyval == gdk::Key::Escape {
                cancel_address_edit(&ui);
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
    }
    ui.recent_list.add_controller(list_keys);
}

/// Connect to the machine in the address bar. The current machine's own
/// endpoint is reused, so this also serves as Reconnect.
fn connect_from_address_bar(ui: &Rc<App>) {
    let machine = ui.entry.text().trim().to_string();
    if machine.is_empty() {
        return;
    }
    let current = ui.endpoint.borrow().clone();
    let endpoint = match current {
        Some(endpoint) if machine_name(&endpoint) == machine => endpoint,
        _ => ssh_endpoint(&ui.cli, &machine),
    };
    connect_to(ui, endpoint);
}

/// Fill the drop-down with the recent machines and show it under the
/// address bar, unless there are none. Popping up under a window the
/// compositor has not shown yet stalls GDK, so until the window is active
/// this does nothing and `install_address_bar` retries on activation.
fn show_recents(ui: &App) {
    while let Some(row) = ui.recent_list.first_child() {
        ui.recent_list.remove(&row);
    }
    let recent = Config::load(&ui.config_path).recent;
    if recent.is_empty() || !ui.window.is_active() {
        ui.recent_popover.popdown();
        return;
    }
    for machine in &recent {
        let row = adw::ActionRow::builder()
            .title(machine)
            .activatable(true)
            .build();
        ui.recent_list.append(&row);
    }
    ui.recent_popover.set_size_request(ui.entry.width(), -1);
    ui.recent_popover.popup();
}

/// Put the address bar back to the current machine and, when there is
/// one, hand focus back to the remote screen.
fn cancel_address_edit(ui: &App) {
    let current = ui.endpoint.borrow().clone();
    ui.entry.set_text(current.as_ref().map_or("", machine_name));
    ui.recent_popover.popdown();
    if current.is_some() {
        ui.video.grab_focus();
    } else {
        ui.entry.grab_focus();
    }
}

/// Make `endpoint` the window's machine and connect to it, dropping any
/// session in progress.
fn connect_to(ui: &Rc<App>, endpoint: Endpoint) {
    let name = machine_name(&endpoint).to_string();
    ui.entry.set_text(&name);
    ui.window.set_title(Some(&window_title(&endpoint)));
    *ui.machine.borrow_mut() = match &endpoint {
        Endpoint::Ssh(_) => Some(name),
        Endpoint::Tcp(_) => None,
    };
    ui.remembered.set(false);
    ui.retries.set(0);
    ui.connect_btn.set_icon_name(CONNECT_ICON);
    ui.connect_btn.set_tooltip_text(Some("Connect"));
    show_disconnected(ui);
    ui.video.grab_focus();
    start_session(ui.clone(), endpoint);
}

/// Go back to a black screen with no stream geometry.
fn show_disconnected(ui: &App) {
    ui.frame.clear();
    ui.stream_size.set((0, 0));
    ui.stats.set_text("");
    ui.video.set_cursor(None);
}

/// Start a worker for `endpoint`. Each call is a new session generation;
/// pollers and reconnects from an older generation stop themselves.
fn start_session(ui: Rc<App>, endpoint: Endpoint) {
    let (frame_tx, frame_rx) = sync_channel::<Frame>(2);
    let (status_tx, status_rx) = channel::<Status>();
    let (input_tx, input_rx) = unbounded_channel::<clipboard::ToWorker>();
    // Replacing the sender closes the old worker's input, which ends it.
    *ui.input_tx.borrow_mut() = Some(input_tx);
    *ui.endpoint.borrow_mut() = Some(endpoint.clone());
    let session = ui.session.get() + 1;
    ui.session.set(session);
    ui.status.set_text("Connecting…");

    let video = gliff_sw::VideoMode::resolve(ui.cli.video.as_deref());
    std::thread::Builder::new()
        .name("gliff-net".into())
        .spawn(move || {
            Worker {
                endpoint,
                video,
                frames: frame_tx,
                status: status_tx,
                input: input_rx,
            }
            .run();
        })
        .expect("spawn network thread");

    poll_frames(ui.clone(), frame_rx, session);
    poll_status(ui, status_rx, session);
}

const MAX_RETRIES: u32 = 5;

/// Put this window's machine at the top of the recent list, once per
/// session. The connected status repeats on every stream reconfigure, such
/// as a resize.
fn remember_machine(ui: &App) {
    let Some(machine) = ui.machine.borrow().clone() else {
        return;
    };
    if ui.remembered.replace(true) {
        return;
    }
    let mut config = Config::load(&ui.config_path);
    config.touch(&machine);
    if let Err(e) = config.save(&ui.config_path) {
        tracing::warn!(path = %ui.config_path.display(), error = %e, "cannot save config");
    }
}

/// Schedule a reconnect after a short delay, unless we have exhausted retries.
fn schedule_reconnect(ui: Rc<App>, session: u64) {
    let n = ui.retries.get() + 1;
    ui.retries.set(n);
    if n > MAX_RETRIES {
        ui.status
            .set_text("Disconnected — press Reconnect to retry");
        ui.connect_btn.set_icon_name(RECONNECT_ICON);
        ui.connect_btn.set_tooltip_text(Some("Reconnect"));
        return;
    }
    let Some(endpoint) = ui.endpoint.borrow().clone() else {
        return;
    };
    ui.status.set_text(&format!("Reconnecting… (attempt {n})"));
    let ui2 = ui.clone();
    glib::timeout_add_local_once(Duration::from_millis(1500), move || {
        if ui2.session.get() == session {
            start_session(ui2, endpoint);
        }
    });
}

/// Pull decoded frames on the GTK main loop, latest-wins, and paint them.
fn poll_frames(ui: Rc<App>, rx: Receiver<Frame>, session: u64) {
    glib::timeout_add_local(Duration::from_millis(8), move || {
        if ui.session.get() != session {
            return glib::ControlFlow::Break;
        }
        let mut latest = None;
        loop {
            match rx.try_recv() {
                Ok(f) => latest = Some(f),
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                // The worker ended; stop this per-session timer so it does not
                // accumulate across reconnects.
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    return glib::ControlFlow::Break
                }
            }
        }
        if let Some(f) = latest {
            match frame_texture(f) {
                Ok(texture) => ui.frame.set_frame(texture, ui.video.scale_factor()),
                Err(e) => tracing::warn!(error = %e, "frame texture import failed"),
            }
        }
        glib::ControlFlow::Continue
    });
}

/// Wrap a decoded frame as a GDK texture: a dmabuf import for the GPU tier,
/// a plain memory texture for the CPU tier.
fn frame_texture(f: Frame) -> Result<gdk::Texture, glib::Error> {
    match f {
        Frame::Dmabuf(f) => dmabuf_texture(f),
        Frame::Bgra(f) => {
            let bytes = glib::Bytes::from_owned(f.pixels);
            Ok(gdk::MemoryTexture::new(
                f.width as i32,
                f.height as i32,
                gdk::MemoryFormat::B8g8r8x8,
                &bytes,
                f.width as usize * 4,
            )
            .upcast())
        }
    }
}

/// Wrap a decoded frame's dmabuf as a GDK texture. The frame (and its fd)
/// lives until GTK releases the texture, which returns the image to the
/// decoder's ring.
fn dmabuf_texture(f: DisplayFrame) -> Result<gdk::Texture, glib::Error> {
    let display = gdk::Display::default()
        .ok_or_else(|| glib::Error::new(glib::FileError::Failed, "no display"))?;
    let builder = gdk::DmabufTextureBuilder::new()
        .set_display(&display)
        .set_width(f.width)
        .set_height(f.height)
        .set_fourcc(f.fourcc as u32)
        .set_modifier(f.modifier)
        .set_n_planes(1)
        .set_offset(0, f.offset)
        .set_stride(0, f.stride)
        .set_premultiplied(false);
    // SAFETY: the fd is a dmabuf describing exactly one linear plane of the
    // stated size, stride and format, and the frame that owns it is kept
    // alive by the release closure until GTK is done with the texture.
    unsafe {
        builder
            .set_fd(0, f.fd.as_raw_fd())
            .build_with_release_func(move || drop(f))
    }
}

fn poll_status(ui: Rc<App>, rx: Receiver<Status>, session: u64) {
    glib::timeout_add_local(Duration::from_millis(100), move || {
        if ui.session.get() != session {
            return glib::ControlFlow::Break;
        }
        while let Ok(s) = rx.try_recv() {
            match s {
                Status::Connected {
                    width,
                    height,
                    scale_milli,
                    video,
                } => {
                    ui.stream_size.set((width, height));
                    ui.stream_scale.set((scale_milli.max(1) as f32) / 1000.0);
                    ui.resize_requested.set((0, 0));
                    ui.retries.set(0);
                    ui.status.set_text(&format!("Connected — {width}x{height}"));
                    // Visible before the first per-second stats arrive.
                    ui.stats.set_text(&video);
                    remember_machine(&ui);
                    // A fresh server starts at its own default size; a
                    // reconnect must bring it back to the window.
                    request_resize(&ui);
                }
                Status::Stats {
                    fps,
                    mbit,
                    decode_ms,
                    video,
                } => {
                    ui.stats.set_text(&format!(
                        "{video}  {fps:.0} fps  {mbit:.1} Mbit/s  decode {decode_ms:.1} ms"
                    ));
                }
                Status::Cursor {
                    width,
                    height,
                    hot_x,
                    hot_y,
                    argb,
                } => set_remote_cursor(&ui, width, height, hot_x, hot_y, &argb),
                Status::ClipboardOffer {
                    serial,
                    mime_types,
                    files,
                } => {
                    if let Some(tx) = ui.input_tx.borrow().clone() {
                        clipboard_ui::set_remote_offer(tx, serial, mime_types, files);
                    }
                }
                Status::ClipboardRead { mime_type, reply } => {
                    clipboard_ui::read_local(mime_type, reply)
                }
                Status::ClipboardTransfer { id, progress } => {
                    let cancel_via = ui.clone();
                    ui.transfers.update(id, &progress, move |id| {
                        if let Some(tx) = cancel_via.input_tx.borrow().clone() {
                            let _ = tx.send(clipboard::ToWorker::CancelTransfer(id));
                        }
                    });
                }
                Status::Error(e) => {
                    tracing::error!(error = %e, "connection failed");
                    ui.status.set_text(&format!("Error: {e}"));
                    show_disconnected(&ui);
                    schedule_reconnect(ui.clone(), session);
                    return glib::ControlFlow::Break;
                }
                Status::Closed => {
                    show_disconnected(&ui);
                    schedule_reconnect(ui.clone(), session);
                    return glib::ControlFlow::Break;
                }
            }
        }
        glib::ControlFlow::Continue
    });
}

/// Show the remote cursor as the video widget's own cursor, so the local
/// compositor draws it at the real pointer position with no added latency.
/// An image with no visible shape (fully transparent, or one flat colour as
/// Hyprland sends when it has no cursor image to share) falls back to the
/// default pointer so the user is never left without one.
fn set_remote_cursor(ui: &App, width: u32, height: u32, hot_x: i32, hot_y: i32, argb: &[u8]) {
    let needed = width as u64 * height as u64 * 4;
    if width == 0 || height == 0 || width > 1024 || height > 1024 || (argb.len() as u64) < needed {
        return;
    }
    if !has_visible_shape(argb) {
        ui.video.set_cursor(None);
        return;
    }
    let bytes = glib::Bytes::from(argb);
    let texture = gdk::MemoryTexture::new(
        width as i32,
        height as i32,
        gdk::MemoryFormat::B8g8r8a8,
        &bytes,
        width as usize * 4,
    );
    let cursor = gdk::Cursor::from_texture(&texture, hot_x, hot_y, None);
    ui.video.set_cursor(Some(&cursor));
}

fn has_visible_shape(argb: &[u8]) -> bool {
    let mut pixels = argb.chunks_exact(4);
    let Some(first) = pixels.next() else {
        return false;
    };
    let opaque = first[3] != 0 || pixels.clone().any(|p| p[3] != 0);
    let uniform = pixels.all(|p| p == first);
    opaque && !uniform
}

/// Map a widget-space point to the remote output's logical coordinates:
/// undo the letterbox to physical stream pixels, then divide by the remote
/// scale, which is what the virtual pointer expects.
fn to_remote(ui: &App, x: f64, y: f64) -> (f64, f64) {
    let (rw, rh) = ui.stream_size.get();
    if rw == 0 || rh == 0 {
        return (0.0, 0.0);
    }
    // The frame is drawn at one stream pixel per device pixel, centred, and
    // only ever shrunk to fit (ScaleDown): its logical size is stream / device
    // scale, times a fit factor of at most 1.
    let device = ui.video.scale_factor().max(1) as f64;
    let (lw, lh) = (rw as f64 / device, rh as f64 / device);
    let (aw, ah) = (ui.video.width() as f64, ui.video.height() as f64);
    let fit = (aw / lw).min(ah / lh).min(1.0);
    let (fw, fh) = (lw * fit, lh * fit);
    let (ox, oy) = ((aw - fw) / 2.0, (ah - fh) / 2.0);
    let px = ((x - ox) / fit * device).clamp(0.0, rw as f64);
    let py = ((y - oy) / fit * device).clamp(0.0, rh as f64);
    let scale = ui.stream_scale.get().max(0.01) as f64;
    (px / scale, py / scale)
}

fn send(ui: &App, msg: ClientMsg) {
    if let Some(tx) = ui.input_tx.borrow().as_ref() {
        let _ = tx.send(clipboard::ToWorker::Send(msg));
    }
}

/// How the user releases captured shortcuts back to the local compositor.
#[derive(Clone)]
enum ReleaseHotkey {
    None,
    DoubleTap {
        keyval: gdk::Key,
        within: Duration,
    },
    Chord {
        mods: gdk::ModifierType,
        keyval: gdk::Key,
    },
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
        if let Some(rest) = lower
            .strip_prefix("double-")
            .or_else(|| lower.strip_prefix("double:"))
        {
            return Ok(Self::DoubleTap {
                keyval: key_from_name(rest)?,
                within: Duration::from_millis(400),
            });
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
    fn matches(
        &self,
        keyval: gdk::Key,
        state: gdk::ModifierType,
        last_tap: &RefCell<Option<Instant>>,
    ) -> bool {
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
            let n = k
                .name()
                .map(|s| s.to_string())
                .unwrap_or_else(|| "?".into());
            if n == "Escape" {
                "Esc".into()
            } else {
                n
            }
        };
        match self {
            Self::None => "release disabled".into(),
            Self::DoubleTap { keyval, .. } => format!("double-tap {}", name(keyval)),
            Self::Chord { mods, keyval } => {
                let mut parts = Vec::new();
                if mods.contains(gdk::ModifierType::CONTROL_MASK) {
                    parts.push("Ctrl".to_string());
                }
                if mods.contains(gdk::ModifierType::ALT_MASK) {
                    parts.push("Alt".to_string());
                }
                if mods.contains(gdk::ModifierType::SHIFT_MASK) {
                    parts.push("Shift".to_string());
                }
                if mods.contains(gdk::ModifierType::SUPER_MASK) {
                    parts.push("Super".to_string());
                }
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
        c.next()
            .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
            .unwrap_or_default()
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
    ui.status
        .set_text("Shortcuts released — click the screen to capture again");
}

/// Release every key held on the remote. Once the video loses focus their
/// local release events never reach us, so the remote would keep (say) Shift
/// down until the next connection.
fn release_pressed_keys(ui: &App) {
    let held = std::mem::take(&mut *ui.pressed_keys.borrow_mut());
    for code in held {
        send(
            ui,
            ClientMsg::Key {
                keycode: code,
                pressed: false,
            },
        );
    }
}

/// Record a local key event. Returns false when it should not be forwarded:
/// an auto-repeat press (the remote compositor repeats on its own) or a
/// release of a key whose press was never sent.
fn track_key(ui: &App, code: u32, pressed: bool) -> bool {
    let mut keys = ui.pressed_keys.borrow_mut();
    if pressed {
        keys.insert(code)
    } else {
        keys.remove(&code)
    }
}

fn install_input_handlers(
    ui: &Rc<App>,
    video: &gtk::Widget,
    window: &adw::ApplicationWindow,
    hotkey: ReleaseHotkey,
) {
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
                release_pressed_keys(&ui);
                release_capture(&ui, &window);
                return glib::Propagation::Stop;
            }
            let code = keycode.saturating_sub(8);
            tracing::debug!(code, "key pressed");
            if track_key(&ui, code, true) {
                send(
                    &ui,
                    ClientMsg::Key {
                        keycode: code,
                        pressed: true,
                    },
                );
            }
            glib::Propagation::Stop
        });
    }
    {
        let ui = ui.clone();
        key.connect_key_released(move |_, _keyval, keycode, _state| {
            let code = keycode.saturating_sub(8);
            if track_key(&ui, code, false) {
                send(
                    &ui,
                    ClientMsg::Key {
                        keycode: code,
                        pressed: false,
                    },
                );
            }
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
            send(
                &ui,
                ClientMsg::PointerButton {
                    button: evdev_button(g.current_button()),
                    pressed: true,
                },
            );
        });
    }
    {
        let ui = ui.clone();
        click.connect_released(move |g, _, _, _| {
            send(
                &ui,
                ClientMsg::PointerButton {
                    button: evdev_button(g.current_button()),
                    pressed: false,
                },
            );
        });
    }
    video.add_controller(click);

    // Scroll.
    let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
    {
        let ui = ui.clone();
        scroll.connect_scroll(move |_, dx, dy| {
            if dy != 0.0 {
                send(
                    &ui,
                    ClientMsg::PointerAxis {
                        axis: Axis::Vertical,
                        value: dy * 15.0,
                        discrete: Some(dy.signum() as i32),
                        stop: false,
                    },
                );
            }
            if dx != 0.0 {
                send(
                    &ui,
                    ClientMsg::PointerAxis {
                        axis: Axis::Horizontal,
                        value: dx * 15.0,
                        discrete: Some(dx.signum() as i32),
                        stop: false,
                    },
                );
            }
            glib::Propagation::Stop
        });
    }
    video.add_controller(scroll);

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
            tracing::debug!("video focused; inhibiting system shortcuts");
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
        let ui = ui.clone();
        focus.connect_leave(move |_| {
            tracing::debug!("video unfocused; restoring system shortcuts");
            release_pressed_keys(&ui);
            if let Some(toplevel) = window.surface().and_downcast::<gdk::Toplevel>() {
                toplevel.restore_system_shortcuts();
            }
        });
    }
    video.add_controller(focus);
}

/// Ask the server to match the window, once the size has settled for 200 ms
/// so a drag-resize does not restart the encoder on every step.
fn install_resize_handler(ui: &Rc<App>) {
    // A Picture has no resize signal, so poll its allocation; the one-shot
    // timer sends only once the size has held for 200 ms.
    let ui = ui.clone();
    glib::timeout_add_local(Duration::from_millis(100), move || {
        let scale = ui.video.scale_factor();
        let (w, h) = (ui.video.width() * scale, ui.video.height() * scale);
        let size = (w.max(0) as u32 & !1, h.max(0) as u32 & !1);
        if size != ui.view_size.get() {
            ui.view_size.set(size);
            let ui = ui.clone();
            glib::timeout_add_local_once(Duration::from_millis(200), move || {
                if ui.view_size.get() == size {
                    request_resize(&ui);
                }
            });
        }
        glib::ControlFlow::Continue
    });
}

/// Send a Resize if the stream does not already match the view.
fn request_resize(ui: &App) {
    let size = ui.view_size.get();
    if size.0 < 64
        || size.1 < 64
        || size == ui.stream_size.get()
        || size == ui.resize_requested.get()
    {
        return;
    }
    ui.resize_requested.set(size);
    send(
        ui,
        ClientMsg::Resize {
            width: size.0,
            height: size.1,
            scale: ui.video.scale_factor() as f32,
        },
    );
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
    fn parse_chord_and_double_and_none() {
        assert!(matches!(
            ReleaseHotkey::parse("none").unwrap(),
            ReleaseHotkey::None
        ));
        assert!(matches!(
            ReleaseHotkey::parse("").unwrap(),
            ReleaseHotkey::None
        ));

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

        assert!(matches!(
            ReleaseHotkey::parse("double-escape").unwrap(),
            ReleaseHotkey::DoubleTap { .. }
        ));
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
        assert!(hk.matches(
            gdk::Key::Escape,
            gdk::ModifierType::SHIFT_MASK | gdk::ModifierType::LOCK_MASK,
            &lt
        ));
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
