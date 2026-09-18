//! Clipboard bridge via `ext-data-control-v1`, on its own thread and Wayland
//! connection. It moves no data itself: it reports which mime types the
//! compositor's selection offers and hands out a pipe to read one of them; in
//! the other direction it advertises a set of mime types on behalf of the
//! remote and hands the owner the pipe of every application that pastes.
//! A loop guard stops a selection we set from being reported back as new.

use std::collections::HashMap;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::mpsc;
use std::time::Duration;

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

#[derive(Debug)]
pub enum ClipboardEvent {
    /// The compositor's selection changed and offers these mime types (empty
    /// when it was cleared). Read one with [`Clipboard::receive`].
    Selection { mime_types: Vec<String> },
    /// An application pastes from the selection set with [`Clipboard::offer`]:
    /// write the item in `mime_type` to `fd` and close it.
    Paste { mime_type: String, fd: OwnedFd },
}

pub type ClipboardSink = Box<dyn FnMut(ClipboardEvent) + Send>;

enum Cmd {
    Offer(Vec<String>),
    Receive {
        mime_type: String,
        reply: mpsc::Sender<Result<OwnedFd>>,
    },
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

    /// Become the compositor's selection, offering `mime_types`; each paste
    /// arrives as [`ClipboardEvent::Paste`]. An empty list withdraws our
    /// selection if we still hold it.
    pub fn offer(&self, mime_types: Vec<String>) {
        let _ = self.cmd.send(Cmd::Offer(mime_types));
    }

    /// Ask the current selection's owner for `mime_type`; the returned pipe
    /// yields the bytes and then EOF.
    pub fn receive(&self, mime_type: String) -> Result<OwnedFd> {
        let (reply, rx) = mpsc::channel();
        self.cmd
            .send(Cmd::Receive { mime_type, reply })
            .map_err(|_| Error::ThreadGone)?;
        rx.recv_timeout(Duration::from_secs(2))
            .map_err(|_| Error::ThreadGone)?
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
    /// the selection's current offer, kept until the selection changes.
    current: Option<ExtDataControlOfferV1>,
    /// our outgoing source and the mimes it advertises.
    our_source: Option<(ExtDataControlSourceV1, Vec<String>)>,
    /// set after we take or drop the selection: the next selection event that
    /// matches is the compositor reporting our own change back to us.
    expect_echo: bool,
    quit: bool,
}

fn spawn(
    target: Target,
    sink: ClipboardSink,
    ready_tx: mpsc::Sender<Result<()>>,
) -> Result<(Sender<Cmd>, std::thread::JoinHandle<()>)> {
    let (tx, rx) = channel::channel::<Cmd>();
    let join = std::thread::Builder::new()
        .name("hypr-clip".into())
        .spawn(move || {
            let mut sink = sink;
            if let Err(e) = run(target, &mut sink, rx, ready_tx.clone()) {
                let _ = ready_tx.send(Err(Error::Input(e.to_string())));
            }
        })?;
    Ok((tx, join))
}

fn run(
    target: Target,
    sink: &mut ClipboardSink,
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
        current: None,
        our_source: None,
        expect_echo: false,
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
            Cmd::Offer(mimes) if mimes.is_empty() => self.withdraw(),
            Cmd::Offer(mimes) => self.set_selection(mimes),
            Cmd::Receive { mime_type, reply } => {
                let _ = reply.send(self.receive(&mime_type));
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
    fn set_selection(&mut self, mimes: Vec<String>) {
        if let Some((old, _)) = self.our_source.take() {
            old.destroy();
        }
        let src = self.manager.create_data_source(&self.qh, ());
        for m in &mimes {
            src.offer(m.clone());
        }
        if let Some(device) = &self.device {
            device.set_selection(Some(&src));
        }
        self.expect_echo = true;
        self.our_source = Some((src, mimes));
    }

    fn withdraw(&mut self) {
        if let Some((old, _)) = self.our_source.take() {
            if let Some(device) = &self.device {
                device.set_selection(None);
            }
            old.destroy();
            self.expect_echo = true;
        }
    }

    fn receive(&mut self, mime: &str) -> Result<OwnedFd> {
        let off = self
            .current
            .as_ref()
            .ok_or_else(|| Error::Input("no selection".into()))?;
        let advertised = self
            .offers
            .get(off)
            .is_some_and(|m| m.iter().any(|a| a == mime));
        if !advertised {
            return Err(Error::Input(format!("selection has no {mime}")));
        }
        let (read_fd, write_fd) = pipe().map_err(|e| Error::Input(e.to_string()))?;
        off.receive(mime.to_string(), write_fd.as_fd());
        drop(write_fd);
        let _ = self.conn.flush();
        Ok(read_fd)
    }

    fn on_selection(&mut self, id: Option<ExtDataControlOfferV1>) {
        // Every offer but the new selection's is stale; destroy them so offer
        // proxies and map entries do not accumulate.
        let stale: Vec<_> = self
            .offers
            .keys()
            .filter(|o| Some(*o) != id.as_ref())
            .cloned()
            .collect();
        for off in stale {
            self.offers.remove(&off);
            off.destroy();
        }
        self.current = id.clone();
        let mimes = id
            .as_ref()
            .and_then(|o| self.offers.get(o))
            .cloned()
            .unwrap_or_default();
        let echo = std::mem::take(&mut self.expect_echo) && self.is_our_echo(&mimes);
        if !echo {
            (self.sink)(ClipboardEvent::Selection { mime_types: mimes });
        }
    }

    /// The compositor reports our own source back to us as a selection whose
    /// offer lists exactly the mimes we gave it; a withdrawal echoes as none.
    fn is_our_echo(&self, mimes: &[String]) -> bool {
        match &self.our_source {
            Some((_, ours)) => same_set(ours, mimes),
            None => mimes.is_empty(),
        }
    }

    fn clear_offers(&mut self) {
        for (off, _) in self.offers.drain() {
            off.destroy();
        }
        self.current = None;
    }
}

fn same_set(a: &[String], b: &[String]) -> bool {
    a.len() == b.len() && a.iter().all(|m| b.contains(m)) && b.iter().all(|m| a.contains(m))
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
            device::Event::Selection { id } => s.on_selection(id),
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
        let ours = s.our_source.as_ref().is_some_and(|(c, _)| c == src);
        match e {
            source::Event::Send { mime_type, fd } if ours => {
                (s.sink)(ClipboardEvent::Paste { mime_type, fd });
            }
            source::Event::Cancelled if ours => {
                s.our_source = None;
            }
            _ => {}
        }
    }
}

delegate_noop!(State: ExtDataControlManagerV1);

#[cfg(test)]
mod tests {
    use super::same_set;

    #[test]
    fn same_set_ignores_order_and_catches_extras() {
        let a = vec!["text/plain".to_string(), "image/png".to_string()];
        let b = vec!["image/png".to_string(), "text/plain".to_string()];
        assert!(same_set(&a, &b));
        assert!(!same_set(&a, &b[..1]));
        assert!(!same_set(
            &a,
            &["image/png".to_string(), "text/html".to_string()]
        ));
        assert!(same_set(&[], &[]));
    }
}
