//! Text clipboard bridge via `ext-data-control-v1`, on its own thread and
//! Wayland connection. It reports the compositor's current text selection and
//! can set the selection from remote data. A loop guard stops a selection we
//! set from bouncing back.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::sync::mpsc;

use calloop::channel::{self, Sender};
use nix::unistd::pipe;
use wayland_client::globals::GlobalListContents;
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::protocol::wl_seat::{self, WlSeat};
use wayland_client::{delegate_noop, Connection, Dispatch, QueueHandle};
use wayland_protocols::ext::data_control::v1::client::ext_data_control_device_v1::{
    self as device, ExtDataControlDeviceV1,
};
use wayland_protocols::ext::data_control::v1::client::ext_data_control_manager_v1::ExtDataControlManagerV1;
use wayland_protocols::ext::data_control::v1::client::ext_data_control_offer_v1::{
    self as offer, ExtDataControlOfferV1,
};
use wayland_protocols::ext::data_control::v1::client::ext_data_control_source_v1::{
    self as source, ExtDataControlSourceV1,
};

use crate::{Error, Result};
use hypr_wl::{LoopState, Outputs, Seat, Target};

/// Text mime types we offer and accept, best first.
const TEXT_MIMES: &[&str] = &[
    "text/plain;charset=utf-8",
    "text/plain",
    "UTF8_STRING",
    "STRING",
];

#[derive(Debug)]
pub enum ClipboardEvent {
    /// The compositor's text selection changed to this value.
    Text(String),
}

pub type ClipboardSink = Box<dyn FnMut(ClipboardEvent) + Send>;

enum Cmd {
    SetText(String),
    Received(String),
    Stop,
}

/// Handle to the clipboard thread.
pub struct Clipboard {
    cmd: Sender<Cmd>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl Clipboard {
    pub fn start(target: Target, sink: ClipboardSink) -> Result<Self> {
        let (ready_tx, ready_rx) = mpsc::channel();
        let (cmd, join) = spawn(target, sink, ready_tx)?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                cmd,
                join: Some(join),
            }),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(Error::Input("clipboard thread gone".into())),
        }
    }

    /// Set the compositor's text selection.
    pub fn set_text(&self, text: String) {
        let _ = self.cmd.send(Cmd::SetText(text));
    }
}

impl Drop for Clipboard {
    fn drop(&mut self) {
        let _ = self.cmd.send(Cmd::Stop);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

struct State {
    sink: ClipboardSink,
    conn: Connection,
    qh: QueueHandle<State>,
    seat: Seat,
    outputs: Outputs,
    manager: ExtDataControlManagerV1,
    device: Option<ExtDataControlDeviceV1>,
    /// mimes advertised by each live incoming offer.
    offers: HashMap<ExtDataControlOfferV1, Vec<String>>,
    /// our outgoing source and the text it serves.
    our_source: Option<(ExtDataControlSourceV1, String)>,
    /// last text we set, to guard against forwarding it back.
    last_set: Option<String>,
    cmd_tx: Sender<Cmd>,
    quit: bool,
}

fn spawn(
    target: Target,
    sink: ClipboardSink,
    ready_tx: mpsc::Sender<Result<()>>,
) -> Result<(Sender<Cmd>, std::thread::JoinHandle<()>)> {
    let (tx, rx) = channel::channel::<Cmd>();
    let tx2 = tx.clone();
    let join = std::thread::Builder::new()
        .name("hypr-clip".into())
        .spawn(move || {
            let mut sink = sink;
            if let Err(e) = run(target, &mut sink, tx2, rx, ready_tx.clone()) {
                let _ = ready_tx.send(Err(Error::Input(e.to_string())));
            }
        })?;
    Ok((tx, join))
}

fn run(
    target: Target,
    sink: &mut ClipboardSink,
    cmd_tx: Sender<Cmd>,
    rx: channel::Channel<Cmd>,
    ready_tx: mpsc::Sender<Result<()>>,
) -> Result<()> {
    let (conn, globals, mut queue) = hypr_wl::init::<State>(&target)?;
    let qh = queue.handle();
    let manager: ExtDataControlManagerV1 = globals
        .bind(&qh, 1..=1, ())
        .map_err(|_| hypr_wl::Error::MissingGlobal("ext_data_control_manager_v1"))?;
    let outputs = Outputs::bind(&globals, &qh)?;
    let seat = Seat::bind(&globals, &qh)?;

    let mut state = State {
        sink: Box::new(|_| {}),
        conn: conn.clone(),
        qh: qh.clone(),
        seat,
        outputs,
        manager,
        device: None,
        offers: HashMap::new(),
        our_source: None,
        last_set: None,
        cmd_tx,
        quit: false,
    };
    std::mem::swap(&mut state.sink, sink);
    queue.roundtrip(&mut state)?;

    let seat = state.seat.wl_seat()?.clone();
    let device = state.manager.get_data_device(&seat, &qh, ());
    state.device = Some(device);
    let _ = ready_tx.send(Ok(()));

    let result = hypr_wl::run_loop(conn, queue, rx, &mut state);
    std::mem::swap(&mut state.sink, sink);
    Ok(result?)
}

impl LoopState for State {
    type Cmd = Cmd;

    fn on_cmd(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::SetText(text) => self.set_selection(text),
            Cmd::Received(text) => {
                // One-shot guard: swallow the echo of the value we set, then
                // let a later genuine copy of the same text through.
                if self.last_set.as_deref() == Some(text.as_str()) {
                    self.last_set = None;
                } else {
                    (self.sink)(ClipboardEvent::Text(text));
                }
            }
            Cmd::Stop => self.stop(),
        }
        let _ = self.conn.flush();
    }

    fn stop(&mut self) {
        self.quit = true;
    }

    fn stop_requested(&self) -> bool {
        self.quit
    }
}

impl State {
    fn set_selection(&mut self, text: String) {
        if let Some((old, _)) = self.our_source.take() {
            old.destroy();
        }
        let src = self.manager.create_data_source(&self.qh, ());
        for m in TEXT_MIMES {
            src.offer((*m).to_string());
        }
        if let Some(device) = &self.device {
            device.set_selection(Some(&src));
        }
        self.last_set = Some(text.clone());
        self.our_source = Some((src, text));
    }

    fn receive_offer(&mut self, off: &ExtDataControlOfferV1) {
        let mimes = self.offers.get(off).cloned().unwrap_or_default();
        let Some(mime) = TEXT_MIMES
            .iter()
            .find(|m| mimes.iter().any(|a| a == *m))
            .map(|m| m.to_string())
        else {
            return; // no text mime offered
        };
        let (read_fd, write_fd) = match pipe() {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "clipboard pipe failed");
                return;
            }
        };
        off.receive(mime, write_fd.as_fd());
        drop(write_fd);
        let _ = self.conn.flush();
        // Read the sender's bytes off the Wayland thread so a slow provider
        // cannot stall dispatch. Bounded by a poll timeout and a size cap so a
        // dead provider that never writes cannot leak the thread and fd.
        let tx = self.cmd_tx.clone();
        std::thread::spawn(move || {
            if let Some(text) = read_selection(read_fd) {
                let _ = tx.send(Cmd::Received(text));
            }
        });
    }

    /// Destroy and forget every tracked offer (called when the selection
    /// changes or clears, so offer proxies and map entries do not accumulate).
    fn clear_offers(&mut self) {
        for (off, _) in self.offers.drain() {
            off.destroy();
        }
    }
}

/// Write our selection to a paste target's pipe, giving up if the target
/// stops draining it for a second, so a stuck target cannot pin a thread.
fn write_selection(fd: OwnedFd, mut data: &[u8]) {
    use nix::poll::{PollFd, PollFlags, PollTimeout};
    let mut file = File::from(fd);
    while !data.is_empty() {
        {
            let borrowed = file.as_fd();
            let mut fds = [PollFd::new(borrowed, PollFlags::POLLOUT)];
            match nix::poll::poll(&mut fds, PollTimeout::from(1000u16)) {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
        }
        match file.write(data) {
            Ok(0) | Err(_) => return,
            Ok(n) => data = &data[n..],
        }
    }
}

/// Read a clipboard selection from a pipe with a bounded wait and size cap.
fn read_selection(read_fd: OwnedFd) -> Option<String> {
    use nix::poll::{PollFd, PollFlags, PollTimeout};
    let mut file = File::from(read_fd);
    let mut buf = Vec::new();
    let max = 32 * 1024 * 1024;
    loop {
        {
            let borrowed = file.as_fd();
            let mut fds = [PollFd::new(borrowed, PollFlags::POLLIN)];
            match nix::poll::poll(&mut fds, PollTimeout::from(1000u16)) {
                Ok(0) | Err(_) => return None, // timed out or errored: give up
                Ok(_) => {}
            }
        }
        let mut chunk = [0u8; 64 * 1024];
        match file.read(&mut chunk) {
            Ok(0) => break, // EOF
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() > max {
                    return None;
                }
            }
            Err(_) => return None,
        }
    }
    String::from_utf8(buf).ok()
}

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: wayland_client::protocol::wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSeat, ()> for State {
    fn event(
        s: &mut Self,
        _: &WlSeat,
        e: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        s.seat.handle(e);
    }
}

impl Dispatch<wayland_client::protocol::wl_output::WlOutput, ()> for State {
    fn event(
        s: &mut Self,
        o: &wayland_client::protocol::wl_output::WlOutput,
        e: wayland_client::protocol::wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        s.outputs.handle(o, e);
    }
}

impl Dispatch<ExtDataControlDeviceV1, ()> for State {
    fn event(
        s: &mut Self,
        _: &ExtDataControlDeviceV1,
        e: device::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match e {
            device::Event::DataOffer { id } => {
                s.offers.insert(id, Vec::new());
            }
            device::Event::Selection { id } => {
                if let Some(off) = &id {
                    s.receive_offer(off);
                }
                // The current offer has been received (or there is none); all
                // tracked offers are now stale, so destroy them.
                s.clear_offers();
            }
            device::Event::Finished => {
                s.clear_offers();
                s.quit = true;
            }
            _ => {}
        }
    }

    wayland_client::event_created_child!(State, ExtDataControlDeviceV1, [
        device::EVT_DATA_OFFER_OPCODE => (ExtDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ExtDataControlOfferV1, ()> for State {
    fn event(
        s: &mut Self,
        off: &ExtDataControlOfferV1,
        e: offer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let offer::Event::Offer { mime_type } = e {
            s.offers.entry(off.clone()).or_default().push(mime_type);
        }
    }
}

impl Dispatch<ExtDataControlSourceV1, ()> for State {
    fn event(
        s: &mut Self,
        src: &ExtDataControlSourceV1,
        e: source::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match e {
            source::Event::Send { mime_type: _, fd } => {
                if let Some((cur, text)) = &s.our_source {
                    if cur == src {
                        // Write off the Wayland thread: a paste target that
                        // reads slowly would otherwise stall dispatch once the
                        // pipe buffer fills.
                        let text = text.clone();
                        std::thread::spawn(move || write_selection(fd, text.as_bytes()));
                    }
                }
            }

            source::Event::Cancelled if s.our_source.as_ref().is_some_and(|(c, _)| c == src) => {
                s.our_source = None;
                s.last_set = None;
            }
            _ => {}
        }
    }
}

delegate_noop!(State: ExtDataControlManagerV1);
