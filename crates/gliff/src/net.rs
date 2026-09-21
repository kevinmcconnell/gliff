//! The client's network and decode worker: connect (TCP or ssh), run the
//! protocol, decode the streams on the GPU, and hand finished display frames
//! (dmabufs) to the GTK thread. Input commands flow the other way.
//!
//! The Vulkan objects live entirely on this worker thread; only dmabuf fds
//! and plain values cross to the GTK thread.

use std::net::SocketAddr;
use std::sync::mpsc::{Sender as StdSender, SyncSender};
use std::sync::Arc;
use std::time::Duration;

use gliff_proto::{ChromaMode, ClientCaps, ClientMsg, Codec, ServerMsg, PROTOCOL_VERSION};
use gliff_transport::recv::{spawn_server_reader, Incoming};
use gliff_transport::udp::Packet;
use gliff_transport::udp_io::{ack as udp_ack, spawn_client, ClientEvent};
use gliff_transport::{spawn_ssh, Framed, SshTarget};
use gliff_vk::{Decoder, DisplayFrame, Gpu};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

/// Status/telemetry the worker reports to the UI.
pub enum Status {
    Connected {
        width: u32,
        height: u32,
        scale_milli: u32,
    },
    Stats {
        fps: f32,
        mbit: f32,
        decode_ms: f32,
        /// Which path carries video, with its loss counters.
        path: String,
    },
    /// The remote cursor image, for the client to set as its widget cursor.
    Cursor {
        width: u32,
        height: u32,
        hot_x: i32,
        hot_y: i32,
        argb: Vec<u8>,
    },
    /// The remote text selection, for the client to put on its local clipboard.
    Clipboard(String),
    Error(String),
    Closed,
}

/// How to reach the server.
#[derive(Clone)]
pub enum Endpoint {
    Tcp(String),
    Ssh(SshTarget),
}

pub struct Worker {
    pub endpoint: Endpoint,
    /// Bounded so a stalled UI thread cannot make the decoder buffer frames
    /// without limit; when full, the newest frame is dropped (latest-wins).
    pub frames: SyncSender<DisplayFrame>,
    pub status: StdSender<Status>,
    pub input: UnboundedReceiver<(ClientMsg, Vec<u8>)>,
    /// Try to take video over UDP when the server offers it.
    pub udp: bool,
}

impl Worker {
    /// Run to completion on the calling thread (spawn it yourself).
    pub fn run(self) {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                let _ = self.status.send(Status::Error(e.to_string()));
                return;
            }
        };
        let local = tokio::task::LocalSet::new();
        let status = self.status.clone();
        let result: anyhow::Result<()> = local.block_on(&rt, async move {
            match self.endpoint {
                Endpoint::Tcp(ref addr) => {
                    let stream = tokio::net::TcpStream::connect(addr).await?;
                    stream.set_nodelay(true)?;
                    let (rd, wr) = tokio::io::split(stream);
                    let host = UdpHost::Tcp(addr.clone());
                    session(rd, wr, self.frames, self.status, self.input, host, self.udp).await
                }
                Endpoint::Ssh(ref target) => {
                    let ssh = spawn_ssh(target)?;
                    let host = UdpHost::Ssh(target.clone());
                    session(ssh.stdout, ssh.stdin, self.frames, self.status, self.input, host, self.udp).await
                }
            }
        });
        if let Err(e) = result {
            let _ = status.send(Status::Error(e.to_string()));
        } else {
            let _ = status.send(Status::Closed);
        }
    }
}

fn new_decoder(
    gpu: &Arc<Gpu>,
    chroma: ChromaMode,
    width: u32,
    height: u32,
) -> anyhow::Result<Decoder> {
    Ok(Decoder::new(
        gpu,
        chroma != ChromaMode::Single420,
        width,
        height,
    )?)
}

/// How to find the server's address for the UDP path.
enum UdpHost {
    /// A `host:port` the connection went to.
    Tcp(String),
    Ssh(SshTarget),
}

/// The server's UDP addresses: the connection's host, or the host ssh
/// resolves the target to (`ssh -G` applies the user's config and aliases).
async fn udp_addrs(host: &UdpHost, port: u16) -> Vec<SocketAddr> {
    let name = match host {
        UdpHost::Tcp(addr) => match tokio::net::lookup_host(addr).await {
            Ok(it) => return it.map(|a| SocketAddr::new(a.ip(), port)).collect(),
            Err(_) => return Vec::new(),
        },
        UdpHost::Ssh(target) => {
            let out = tokio::process::Command::new("ssh")
                .arg("-G")
                .args(&target.ssh_args)
                .arg(&target.host)
                .output()
                .await;
            let Ok(out) = out else { return Vec::new() };
            let text = String::from_utf8_lossy(&out.stdout);
            let Some(name) = text
                .lines()
                .find_map(|l| l.strip_prefix("hostname "))
                .map(|h| h.trim().to_string())
            else {
                return Vec::new();
            };
            name
        }
    };
    match tokio::net::lookup_host((name, port)).await {
        Ok(it) => it.collect(),
        Err(e) => {
            tracing::debug!(error = %e, "cannot resolve the UDP host");
            Vec::new()
        }
    }
}

/// The decode side of the session: frames from either path go through the
/// same gate, decoder, ack and recovery.
struct Receiver {
    decoder: Decoder,
    /// Frames decoded and possibly still in the decoder's reference
    /// buffer: a frame that predicts from anything else cannot be decoded.
    decoded_ids: std::collections::VecDeque<u64>,
    frames: SyncSender<DisplayFrame>,
    tcp: UnboundedSender<(ClientMsg, Vec<u8>)>,
    udp: Option<UnboundedSender<Packet>>,
    udp_active: bool,
    logged_first: bool,
    frames_since: u32,
    bytes_since: u64,
    decode_ms_acc: f32,
}

impl Receiver {
    fn ack(&self, frame_id: u64, held_ms: u32) {
        match (&self.udp, self.udp_active) {
            (Some(udp), true) => {
                let _ = udp.send(udp_ack(frame_id, held_ms));
            }
            _ => {
                let _ = self.tcp.send((
                    ClientMsg::FrameAck {
                        frame_id,
                        decoded_at_ms: now_ms(),
                        held_ms,
                    },
                    Vec::new(),
                ));
            }
        }
    }

    fn recover(&self, frame_id: u64, reference: Option<u64>, what: &str) {
        let last_good = self.decoded_ids.back().copied();
        tracing::warn!(frame_id, ?reference, last_good, "{what}; asking for recovery");
        match (&self.udp, self.udp_active) {
            (Some(udp), true) => {
                let _ = udp.send(Packet::Recover { last_good });
            }
            _ => {
                let _ = self
                    .tcp
                    .send((ClientMsg::Recover { last_good }, Vec::new()));
            }
        }
    }

    fn on_frame(
        &mut self,
        frame_id: u64,
        keyframe: bool,
        reference: Option<u64>,
        main: &[u8],
        aux: &[u8],
        held_ms: u32,
    ) {
        self.bytes_since += (main.len() + aux.len()) as u64;
        let have_reference =
            keyframe || reference.is_some_and(|r| self.decoded_ids.contains(&r));
        let t0 = std::time::Instant::now();
        let decoded = if have_reference {
            self.decoder.decode(main, aux)
        } else {
            Ok(None)
        };
        let dec_ms = t0.elapsed().as_secs_f32() * 1000.0;
        // Ack immediately so the server keeps pacing.
        self.ack(frame_id, held_ms);
        if !have_reference {
            self.recover(frame_id, reference, "reference frame missing");
        }
        match decoded {
            Ok(Some(frame)) => {
                if !self.logged_first {
                    tracing::info!("first frame decoded");
                    self.logged_first = true;
                }
                if keyframe {
                    self.decoded_ids.clear();
                }
                self.decoded_ids.push_back(frame_id);
                if self.decoded_ids.len() > gliff_vk::MAX_REFERENCES {
                    self.decoded_ids.pop_front();
                }
                // Latest-wins: drop this frame if the UI hasn't drained.
                let _ = self.frames.try_send(frame);
                self.frames_since += 1;
                self.decode_ms_acc += dec_ms;
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(error = %e, "decode error");
                self.recover(frame_id, reference, "decode error");
            }
        }
    }

    fn set_path(&mut self, udp: bool) {
        if udp != self.udp_active {
            self.udp_active = udp;
            tracing::info!(path = if udp { "udp" } else { "tcp" }, "video path");
            let _ = self
                .tcp
                .send((ClientMsg::VideoPath { udp }, Vec::new()));
        }
    }
}

async fn session<R, W>(
    rd: R,
    wr: W,
    frames: SyncSender<DisplayFrame>,
    status: StdSender<Status>,
    mut input: UnboundedReceiver<(ClientMsg, Vec<u8>)>,
    udp_host: UdpHost,
    udp_wanted: bool,
) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin + 'static,
    W: AsyncWrite + Unpin + 'static,
{
    let mut reader = Framed::new(rd);
    let mut writer = Framed::new(wr);

    let caps = ClientCaps {
        codecs: vec![Codec::H264],
        max_width: 3840,
        max_height: 2160,
        chroma: vec![ChromaMode::Dual420, ChromaMode::Single420],
        udp: udp_wanted,
    };
    let keymap = crate::keymap::local_keymap();
    writer
        .write_msg(&ClientMsg::Hello {
            version: PROTOCOL_VERSION,
            keymap,
            caps,
        })
        .await?;

    let ack = reader.read_msg::<ServerMsg>().await?;
    let ServerMsg::HelloAck { udp: udp_offer, .. } = ack else {
        anyhow::bail!("expected HelloAck, got {ack:?}");
    };
    let cfg = reader.read_msg::<ServerMsg>().await?;
    let (width, height, chroma, scale_milli) = match cfg {
        ServerMsg::StreamConfig {
            width,
            height,
            chroma,
            scale_milli,
            ..
        } => (width, height, chroma, scale_milli),
        other => anyhow::bail!("expected StreamConfig, got {other:?}"),
    };
    let gpu = Gpu::open(Some(&hypr_capture::render_node(None)))?;
    let decoder = new_decoder(&gpu, chroma, width, height)?;
    let _ = status.send(Status::Connected {
        width,
        height,
        scale_milli,
    });
    tracing::info!(width, height, scale_milli, gpu = %gpu.name, "connected");

    // Writes run on their own task, fed by `out_tx`, so reads (draining video)
    // never block on a write and the two peers cannot deadlock. Both the reader
    // loop (acks, keyframe requests) and the UI thread (input) feed `out_tx`.
    let (out_tx, mut out_rx) = unbounded_channel::<(ClientMsg, Vec<u8>)>();
    tokio::task::spawn_local(async move {
        while let Some((m, payload)) = out_rx.recv().await {
            let r = if payload.is_empty() {
                writer.write_msg(&m).await
            } else {
                writer.write_msg_with_payloads(&m, &[&payload]).await
            };
            if r.is_err() {
                break;
            }
        }
    });
    // Forward UI input into the same write channel.
    {
        let out_tx = out_tx.clone();
        tokio::task::spawn_local(async move {
            while let Some(mp) = input.recv().await {
                if out_tx.send(mp).is_err() {
                    break;
                }
            }
        });
    }

    // The UDP path: probe the server's addresses from a thread of its own;
    // video moves there once it answers, and back here if it stops.
    let mut udp_rx = None;
    let mut udp_tx = None;
    if let Some(offer) = udp_offer {
        let addrs = udp_addrs(&udp_host, offer.port).await;
        if addrs.is_empty() {
            tracing::info!("UDP path offered but the host does not resolve; staying on the connection");
        } else {
            tracing::info!(?addrs, "probing the UDP path");
            let (rx, tx) = spawn_client(addrs, offer.key, Duration::from_secs(6));
            udp_rx = Some(rx);
            udp_tx = Some(tx);
        }
    }
    let mut recv = Receiver {
        decoder,
        decoded_ids: std::collections::VecDeque::new(),
        frames,
        tcp: out_tx.clone(),
        udp: udp_tx,
        udp_active: false,
        logged_first: false,
        frames_since: 0,
        bytes_since: 0,
        decode_ms_acc: 0.0,
    };
    let mut path_stats = String::from("tcp");
    let mut tcp_rx = spawn_server_reader(reader);
    let mut last_report = std::time::Instant::now();

    loop {
        tokio::select! {
            inc = tcp_rx.recv() => {
                let Some(Incoming { msg, main, aux }) = inc else { break };
                match msg {
                    ServerMsg::VideoFrame {
                        frame_id,
                        keyframe,
                        reference,
                        ..
                    } => recv.on_frame(frame_id, keyframe, reference, &main, &aux, 0),
                    ServerMsg::StreamConfig {
                        width,
                        height,
                        chroma,
                        scale_milli,
                        ..
                    } => {
                        recv.decoder = new_decoder(&gpu, chroma, width, height)?;
                        recv.decoded_ids.clear();
                        let _ = status.send(Status::Connected {
                            width,
                            height,
                            scale_milli,
                        });
                    }
                    ServerMsg::CursorShape {
                        width,
                        height,
                        hot_x,
                        hot_y,
                        ..
                    } => {
                        let _ = status.send(Status::Cursor {
                            width,
                            height,
                            hot_x,
                            hot_y,
                            argb: main.to_vec(),
                        });
                    }
                    ServerMsg::ClipboardData { .. } => {
                        if let Ok(text) = String::from_utf8(main.to_vec()) {
                            let _ = status.send(Status::Clipboard(text));
                        }
                    }
                    ServerMsg::CursorPos { .. } | ServerMsg::Pong { .. } => {}
                    ServerMsg::Error { code, message } => anyhow::bail!("server error {code}: {message}"),
                    ServerMsg::HelloAck { .. }
                    | ServerMsg::ClipboardOffer { .. }
                    | ServerMsg::ClipboardRequest { .. } => {}
                }
            }
            ev = async {
                match udp_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                match ev {
                    Some(ClientEvent::Up { rtt }) => {
                        tracing::info!(rtt_ms = rtt.as_secs_f64() * 1000.0, "UDP path is up");
                        recv.set_path(true);
                        path_stats = "udp".into();
                    }
                    Some(ClientEvent::Frame(f)) => {
                        recv.on_frame(f.info.frame_id, f.info.keyframe, f.info.reference, &f.main, &f.aux, f.held.as_millis() as u32);
                    }
                    Some(ClientEvent::Lost(frame_id)) => {
                        recv.recover(frame_id, None, "frame lost on the UDP path");
                    }
                    Some(ClientEvent::Stats(s)) => {
                        path_stats = format!("udp lost {} repaired {}", s.lost, s.repaired);
                    }
                    Some(ClientEvent::Down) | None => {
                        tracing::warn!("UDP path is down; video stays on the connection");
                        recv.set_path(false);
                        recv.udp = None;
                        udp_rx = None;
                        path_stats = "tcp".into();
                    }
                }
            }
        }
        if last_report.elapsed().as_secs_f32() >= 1.0 {
            let secs = last_report.elapsed().as_secs_f32();
            let _ = status.send(Status::Stats {
                fps: recv.frames_since as f32 / secs,
                mbit: recv.bytes_since as f32 * 8.0 / 1_000_000.0 / secs,
                decode_ms: if recv.frames_since > 0 {
                    recv.decode_ms_acc / recv.frames_since as f32
                } else {
                    0.0
                },
                path: path_stats.clone(),
            });
            recv.frames_since = 0;
            recv.bytes_since = 0;
            recv.decode_ms_acc = 0.0;
            last_report = std::time::Instant::now();
        }
    }
    Ok(())
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
