//! The UDP video path on the wire: each end runs its socket on a thread of
//! its own, so a frame being encoded or decoded never delays a NACK, an
//! ack or a retransmit, and talks to the session through channels.

use std::net::{SocketAddr, UdpSocket as StdUdpSocket};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::net::UdpSocket;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use crate::udp::{
    split_frame, AssembledFrame, Assembler, Event, FrameInfo, Opener, Packet, PartCache, Sealer,
    Stats, DIR_CLIENT, DIR_SERVER, KEY_LEN, MAX_DATAGRAM,
};

/// Socket buffers large enough for a burst of frames while the other
/// thread is busy; the kernel caps them at its maximum.
const SOCKET_BUFFER: usize = 8 << 20;
const PROBE_EVERY: Duration = Duration::from_millis(500);
/// Probes without an answer before the path counts as down.
const PROBES_LOST: u32 = 4;
const TICK: Duration = Duration::from_millis(4);

fn bind(addr: SocketAddr) -> std::io::Result<StdUdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let domain = if addr.is_ipv6() {
        Domain::IPV6
    } else {
        Domain::IPV4
    };
    let socket = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
    if addr.is_ipv6() {
        socket.set_only_v6(false).ok();
    }
    socket.set_send_buffer_size(SOCKET_BUFFER).ok();
    socket.set_recv_buffer_size(SOCKET_BUFFER).ok();
    socket.set_nonblocking(true)?;
    socket.bind(&addr.into())?;
    Ok(socket.into())
}

fn now_us(start: Instant) -> u64 {
    start.elapsed().as_micros() as u64
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// What the client's UDP thread reports.
#[derive(Debug)]
pub enum ClientEvent {
    /// The server answered a probe; video may move to UDP.
    Up { rtt: Duration },
    Frame(AssembledFrame),
    Lost(u64),
    /// Probes go unanswered, or the probe never got an answer: use TCP.
    Down,
    /// Periodic counters, for the display.
    Stats(Stats),
}

/// Start the client's UDP thread. It probes every address until one
/// answers, within `timeout`, then reports `Up`; frames and losses follow.
/// Packets sent into the returned sender go to the server.
pub fn spawn_client(
    addrs: Vec<SocketAddr>,
    key: [u8; KEY_LEN],
    timeout: Duration,
) -> (UnboundedReceiver<ClientEvent>, UnboundedSender<Packet>) {
    let (ev_tx, ev_rx) = unbounded_channel();
    let (pk_tx, pk_rx) = unbounded_channel();
    std::thread::Builder::new()
        .name("gliff-udp".into())
        .spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                let _ = ev_tx.send(ClientEvent::Down);
                return;
            };
            rt.block_on(async move {
                match client_connect(&addrs, &key, timeout).await {
                    Some((socket, peer, rtt)) => {
                        let _ = ev_tx.send(ClientEvent::Up { rtt });
                        client_loop(socket, peer, key, rtt, ev_tx, pk_rx).await;
                    }
                    None => {
                        let _ = ev_tx.send(ClientEvent::Down);
                    }
                }
            });
        })
        .expect("spawn UDP thread");
    (ev_rx, pk_tx)
}

/// Probe every address from a socket of its family; the first answer
/// picks the socket and the peer.
async fn client_connect(
    addrs: &[SocketAddr],
    key: &[u8; KEY_LEN],
    timeout: Duration,
) -> Option<(UdpSocket, SocketAddr, Duration)> {
    let start = Instant::now();
    let mut sockets = Vec::new();
    for family_v6 in [false, true] {
        let targets: Vec<SocketAddr> = addrs
            .iter()
            .copied()
            .filter(|a| a.is_ipv6() == family_v6)
            .collect();
        if targets.is_empty() {
            continue;
        }
        let local: SocketAddr = if family_v6 {
            "[::]:0".parse().unwrap()
        } else {
            "0.0.0.0:0".parse().unwrap()
        };
        match bind(local).and_then(UdpSocket::from_std) {
            Ok(s) => sockets.push((s, targets)),
            Err(e) => tracing::debug!(error = %e, "no UDP socket for this family"),
        }
    }
    let mut sealer = Sealer::new(key, DIR_CLIENT);
    let mut opener = Opener::new(key, DIR_SERVER);
    let deadline = start + timeout;
    let mut buf = vec![0u8; MAX_DATAGRAM + 64];
    let mut sent_at: Vec<(u64, Instant)> = Vec::new();
    while Instant::now() < deadline {
        let t = now_us(start).max(1);
        let probe = sealer.seal_packet(&Packet::Probe { t });
        sent_at.push((t, Instant::now()));
        for (socket, targets) in &sockets {
            for target in targets {
                let _ = socket.try_send_to(&probe, *target);
            }
        }
        let wait = tokio::time::sleep(Duration::from_millis(150));
        tokio::pin!(wait);
        loop {
            // Poll every socket; the first authenticated answer wins.
            let mut any = None;
            for (i, (socket, targets)) in sockets.iter().enumerate() {
                tokio::select! {
                    r = socket.recv_from(&mut buf) => {
                        if let Ok((n, from)) = r {
                            if targets.contains(&from) {
                                if let Some((Packet::ProbeAck { t }, _)) = opener.open(&buf[..n]) {
                                    let rtt = sent_at
                                        .iter()
                                        .find(|(pt, _)| *pt == t)
                                        .map(|(_, at)| at.elapsed())
                                        .unwrap_or(Duration::from_millis(50));
                                    any = Some((i, from, rtt));
                                }
                            }
                        }
                    }
                    _ = &mut wait => break,
                }
                if any.is_some() {
                    break;
                }
            }
            if let Some((i, peer, rtt)) = any {
                let (socket, _) = sockets.swap_remove(i);
                return Some((socket, peer, rtt));
            }
            if wait.is_elapsed() || sockets.is_empty() {
                break;
            }
        }
        if sockets.is_empty() {
            break;
        }
    }
    None
}

async fn client_loop(
    socket: UdpSocket,
    peer: SocketAddr,
    key: [u8; KEY_LEN],
    rtt: Duration,
    ev_tx: UnboundedSender<ClientEvent>,
    mut pk_rx: UnboundedReceiver<Packet>,
) {
    let start = Instant::now();
    let mut sealer = Sealer::new(&key, DIR_CLIENT);
    let mut opener = Opener::new(&key, DIR_SERVER);
    let mut asm = Assembler::new();
    asm.set_rtt(rtt);
    let mut rtt_ms = rtt.as_secs_f64() * 1000.0;
    let mut buf = vec![0u8; MAX_DATAGRAM + 64];
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut probe = tokio::time::interval(PROBE_EVERY);
    let mut probes_out: Vec<(u64, Instant)> = Vec::new();
    let mut last_stats = Instant::now();
    // One probe right away, so the server's idea of the peer is this socket.
    let t = now_us(start).max(1);
    let _ = socket.try_send_to(&sealer.seal_packet(&Packet::Probe { t }), peer);
    probes_out.push((t, Instant::now()));
    loop {
        let events = tokio::select! {
            r = socket.recv_from(&mut buf) => {
                let Ok((n, from)) = r else { continue };
                if from != peer {
                    continue;
                }
                let Some((packet, bytes)) = opener.open(&buf[..n]) else { continue };
                match packet {
                    Packet::ProbeAck { t } => {
                        if let Some(pos) = probes_out.iter().position(|(pt, _)| *pt == t) {
                            let sample = probes_out[pos].1.elapsed().as_secs_f64() * 1000.0;
                            probes_out.clear();
                            rtt_ms = 0.8 * rtt_ms + 0.2 * sample;
                            asm.set_rtt(Duration::from_secs_f64(rtt_ms / 1000.0));
                        }
                        Vec::new()
                    }
                    Packet::Part { .. } => asm.on_packet(packet, bytes, Instant::now()),
                    _ => Vec::new(),
                }
            }
            _ = tick.tick() => {
                if asm.pending() > 0 { asm.tick(Instant::now()) } else { Vec::new() }
            }
            _ = probe.tick() => {
                if probes_out.len() as u32 >= PROBES_LOST {
                    tracing::warn!("UDP path: no answer to {} probes; giving it up", probes_out.len());
                    let _ = ev_tx.send(ClientEvent::Down);
                    return;
                }
                let t = now_us(start).max(1);
                let _ = socket.try_send_to(&sealer.seal_packet(&Packet::Probe { t }), peer);
                probes_out.push((t, Instant::now()));
                if last_stats.elapsed() >= Duration::from_secs(1) {
                    last_stats = Instant::now();
                    let _ = ev_tx.send(ClientEvent::Stats(asm.stats));
                }
                Vec::new()
            }
            p = pk_rx.recv() => {
                let Some(p) = p else { return };
                let _ = socket.try_send_to(&sealer.seal_packet(&p), peer);
                Vec::new()
            }
        };
        for p in asm.outgoing.drain(..) {
            let _ = socket.try_send_to(&sealer.seal_packet(&p), peer);
        }
        for e in events {
            let ev = match e {
                Event::Frame(f) => ClientEvent::Frame(f),
                Event::Lost(id) => ClientEvent::Lost(id),
            };
            if ev_tx.send(ev).is_err() {
                return;
            }
        }
    }
}

/// A frame for the server's UDP thread to send.
pub struct OutFrame {
    pub info: FrameInfo,
    pub main: Vec<u8>,
    pub aux: Vec<u8>,
}

/// The server's end: a socket on any port (or the one asked for) and a
/// fresh session key, both to be handed to the client over SSH.
pub struct UdpServer {
    socket: StdUdpSocket,
    key: [u8; KEY_LEN],
    port: u16,
}

impl UdpServer {
    pub fn bind(port: Option<u16>) -> std::io::Result<Self> {
        let port = port.unwrap_or(0);
        let socket = bind(SocketAddr::from(([0u8; 16], port)))
            .or_else(|_| bind(SocketAddr::from(([0u8; 4], port))))?;
        let port = socket.local_addr()?.port();
        let mut key = [0u8; KEY_LEN];
        rand::fill(&mut key);
        Ok(Self { socket, key, port })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn key(&self) -> [u8; KEY_LEN] {
        self.key
    }

    /// Start the thread. Frames sent into the returned sender go to the
    /// client once it has probed; `queued` is decremented as each leaves.
    /// The client's acks and recovery requests come out of the receiver.
    pub fn spawn(self, queued: Arc<AtomicU32>) -> (UnboundedSender<OutFrame>, UnboundedReceiver<Packet>) {
        let (fr_tx, fr_rx) = unbounded_channel();
        let (pk_tx, pk_rx) = unbounded_channel();
        std::thread::Builder::new()
            .name("gliff-udp".into())
            .spawn(move || {
                let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    return;
                };
                rt.block_on(server_loop(self, queued, fr_rx, pk_tx));
            })
            .expect("spawn UDP thread");
        (fr_tx, pk_rx)
    }
}

async fn server_loop(
    server: UdpServer,
    queued: Arc<AtomicU32>,
    mut fr_rx: UnboundedReceiver<OutFrame>,
    pk_tx: UnboundedSender<Packet>,
) {
    let Ok(socket) = UdpSocket::from_std(server.socket) else {
        return;
    };
    let mut sealer = Sealer::new(&server.key, DIR_SERVER);
    let mut opener = Opener::new(&server.key, DIR_CLIENT);
    let mut cache = PartCache::new();
    let mut peer: Option<SocketAddr> = None;
    let mut last_probe_t = 0u64;
    let mut buf = vec![0u8; MAX_DATAGRAM + 64];
    let mut sent_frames = 0u64;
    let mut retransmits = 0u64;
    loop {
        tokio::select! {
            f = fr_rx.recv() => {
                let Some(f) = f else { return };
                if let Some(peer) = peer {
                    let parts = split_frame(&f.info, &f.main, &f.aux);
                    for p in &parts {
                        if let Err(e) = socket.try_send_to(&sealer.seal(p), peer) {
                            tracing::debug!(error = %e, "UDP send failed; the client will ask again");
                        }
                    }
                    cache.insert(f.info.frame_id, parts);
                    sent_frames += 1;
                }
                queued.fetch_sub(1, Ordering::AcqRel);
            }
            r = socket.recv_from(&mut buf) => {
                let Ok((n, from)) = r else { continue };
                let Some((packet, _)) = opener.open(&buf[..n]) else { continue };
                match packet {
                    Packet::Probe { t } => {
                        // Only a newer probe may move the peer, so a replayed
                        // one cannot redirect the stream.
                        if t > last_probe_t {
                            last_probe_t = t;
                            if peer != Some(from) {
                                tracing::info!(%from, "UDP peer");
                                peer = Some(from);
                            }
                        }
                        if peer == Some(from) {
                            let _ = socket.try_send_to(&sealer.seal_packet(&Packet::ProbeAck { t }), from);
                        }
                    }
                    Packet::Nack { frame_id, missing } if peer == Some(from) => {
                        for i in missing {
                            if let Some(p) = cache.part(frame_id, i) {
                                let _ = socket.try_send_to(&sealer.seal(p), from);
                                retransmits += 1;
                            }
                        }
                        tracing::debug!(frame_id, sent_frames, retransmits, "retransmitted");
                    }
                    Packet::Ack { .. } | Packet::Recover { .. } if peer == Some(from) => {
                        if pk_tx.send(packet).is_err() {
                            return;
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

/// The client's ack for a decoded frame, as the UDP thread sends it.
pub fn ack(frame_id: u64) -> Packet {
    Packet::Ack {
        frame_id,
        decoded_at_ms: now_ms(),
    }
}
