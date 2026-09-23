//! The client's network and decode worker: connect (TCP or ssh), run the
//! protocol, decode the streams on the GPU, and hand finished display frames
//! (dmabufs) to the GTK thread. Input commands flow the other way.
//!
//! The Vulkan objects live entirely on this worker thread; only dmabuf fds
//! and plain values cross to the GTK thread.

use std::rc::Rc;
use std::sync::mpsc::{Sender as StdSender, SyncSender};
use std::sync::Arc;

use bytes::Bytes;
use gliff_proto::clipboard::CHUNK;
use gliff_proto::{
    ChromaMode, ClientCaps, ClientMsg, ClipboardFile, ClipboardMsg, Codec, ServerMsg,
    PROTOCOL_VERSION,
};
use gliff_sw::VideoMode;
use gliff_transport::clipboard::progress::Progress;
use gliff_transport::clipboard::{outbound_channel, Side, Transfers};
use gliff_transport::{spawn_ssh, Framed, SshTarget};
use gliff_vk::{Decoder, DisplayFrame, Gpu};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc::{self, unbounded_channel, UnboundedReceiver};

use crate::clipboard::{Bridge, ToWorker};
use crate::keymap::{Keymap, FIRST_KEYMAP_WAIT};

/// Stream geometry one picture was configured under: stream size, view
/// size, and the effective scale x1000.
type Geometry = ((u32, u32), (u32, u32), u32);

/// A decoded frame with the geometry it was configured under, so the UI
/// never paints a frame with another configuration's view or scale (frames
/// and statuses travel on separate channels).
pub struct Picture {
    pub frame: Frame,
    /// Stream size in physical pixels.
    pub stream: (u32, u32),
    /// The full-quality fit size the frame should be drawn into.
    pub view: (u32, u32),
    /// Effective stream scale x1000 for pointer mapping.
    pub scale_milli: u32,
}

/// Status/telemetry the worker reports to the UI. Pointer-mapping geometry
/// is deliberately absent: it travels with each `Picture`, so the UI never
/// maps clicks against a configuration whose frame is not on screen yet.
pub enum Status {
    Connected {
        /// "server pipeline > client pipeline", e.g. "gpu > cpu".
        video: String,
        view_width: u32,
        view_height: u32,
        fps_cap: u32,
    },
    Stats {
        fps: f32,
        mbit: f32,
        decode_ms: f32,
        /// "server pipeline > client pipeline", e.g. "gpu > cpu".
        video: String,
    },
    /// The remote cursor image, for the client to set as its widget cursor.
    Cursor {
        width: u32,
        height: u32,
        hot_x: i32,
        hot_y: i32,
        argb: Vec<u8>,
    },
    /// The remote selection changed: put a proxy for these on the local
    /// clipboard, or clear ours when both lists are empty. Pastes from the
    /// proxy echo `serial`, so one racing a newer offer is refused.
    ClipboardOffer {
        serial: u32,
        mime_types: Vec<String>,
        files: Vec<ClipboardFile>,
    },
    /// The server wants `mime_type` from the local clipboard: read it into
    /// `reply` chunk by chunk and drop the sender at the end.
    ClipboardRead {
        mime_type: String,
        reply: mpsc::Sender<std::io::Result<Bytes>>,
    },
    /// A paste from the server's offer that is taking a while, or ended.
    ClipboardTransfer {
        id: u32,
        progress: Progress,
    },
    Error(String),
    Closed,
}

/// How to reach the server.
#[derive(Clone)]
pub enum Endpoint {
    Tcp(String),
    Ssh(SshTarget),
}

/// A decoded frame for the GTK thread: a dmabuf from the GPU tier, or plain
/// BGRA pixels from the CPU tier.
pub enum Frame {
    Dmabuf(DisplayFrame),
    Bgra(gliff_sw::BgraFrame),
}

pub struct Worker {
    pub endpoint: Endpoint,
    pub video: VideoMode,
    /// Bounded so a stalled UI thread cannot make the decoder buffer frames
    /// without limit; when full, the newest frame is dropped (latest-wins).
    pub frames: SyncSender<Picture>,
    pub status: StdSender<Status>,
    pub input: UnboundedReceiver<ToWorker>,
    pub keymap: Keymap,
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
                    session(
                        rd,
                        wr,
                        self.video,
                        self.frames,
                        self.status,
                        self.input,
                        self.keymap,
                    )
                    .await
                }
                Endpoint::Ssh(ref target) => {
                    let ssh = spawn_ssh(target)?;
                    session(
                        ssh.stdout,
                        ssh.stdin,
                        self.video,
                        self.frames,
                        self.status,
                        self.input,
                        self.keymap,
                    )
                    .await
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

/// The decode pipeline: Vulkan Video on the GPU, or OpenH264 on the CPU.
enum VideoDecoder {
    Gpu(Box<Decoder>),
    Cpu(Box<gliff_sw::Decoder>),
}

/// Open the GPU for decoding, or `None` for the CPU tier. In `Gpu` mode a
/// machine without a usable Vulkan Video decoder falls back to the CPU.
fn open_gpu(mode: VideoMode) -> Option<Arc<Gpu>> {
    if mode == VideoMode::Cpu {
        tracing::info!("using the CPU video pipeline as requested");
        return None;
    }
    match Gpu::open(Some(&hypr_capture::render_node(None))) {
        Ok(gpu) if gpu.can_decode() => Some(gpu),
        Ok(gpu) => {
            tracing::warn!(gpu = %gpu.name, "no Vulkan H.264 decode queue; falling back to the CPU pipeline");
            None
        }
        Err(e) => {
            tracing::warn!(error = %e, "no usable Vulkan device; falling back to the CPU pipeline");
            None
        }
    }
}

fn new_decoder(
    gpu: &Option<Arc<Gpu>>,
    chroma: ChromaMode,
    width: u32,
    height: u32,
) -> anyhow::Result<VideoDecoder> {
    let dual = chroma != ChromaMode::Single420;
    Ok(match gpu {
        Some(gpu) => VideoDecoder::Gpu(Box::new(Decoder::new(gpu, dual, width, height)?)),
        None => VideoDecoder::Cpu(Box::new(gliff_sw::Decoder::new(dual)?)),
    })
}

async fn session<R, W>(
    rd: R,
    wr: W,
    video: VideoMode,
    frames: SyncSender<Picture>,
    status: StdSender<Status>,
    mut input: UnboundedReceiver<ToWorker>,
    mut keymap: Keymap,
) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin + 'static,
    W: AsyncWrite + Unpin + 'static,
{
    let mut reader = Framed::new(rd);
    let mut writer = Framed::new(wr);

    let gpu = open_gpu(video);
    let caps = ClientCaps {
        codecs: vec![Codec::H264],
        max_width: 3840,
        max_height: 2160,
        // The CPU tier asks for one 4:2:0 stream so it decodes one stream,
        // not two; the recombine also costs CPU on this side.
        chroma: if gpu.is_some() {
            vec![ChromaMode::Dual420, ChromaMode::Single420]
        } else {
            vec![ChromaMode::Single420]
        },
    };
    let _ = tokio::time::timeout(FIRST_KEYMAP_WAIT, keymap.wait_for(|k| !k.is_empty())).await;
    let first_keymap = keymap.borrow_and_update().clone();
    if first_keymap.is_empty() {
        tracing::warn!("no local keymap yet; server will default to us");
    }
    tracing::debug!(bytes = first_keymap.len(), "sending keymap");
    writer
        .write_msg(&ClientMsg::Hello {
            version: PROTOCOL_VERSION,
            keymap: first_keymap,
            caps,
        })
        .await?;

    // Between HelloAck and StreamConfig the server may send a Ping; answer
    // it before anything else (even GPU setup) so the server's round-trip
    // estimate is seeded ahead of the first frame ack.
    let mut got_ack = false;
    let (mut width, mut height, mut chroma, mut scale_milli, pipeline, mut view, mut fps_cap) = loop {
        match reader.read_msg::<ServerMsg>().await? {
            ServerMsg::HelloAck { version, .. } => {
                if version != PROTOCOL_VERSION {
                    anyhow::bail!("server version {version} != {PROTOCOL_VERSION}");
                }
                got_ack = true;
            }
            ServerMsg::Ping { t } => writer.write_msg(&ClientMsg::Pong { t }).await?,
            ServerMsg::StreamConfig {
                width,
                height,
                chroma,
                scale_milli,
                pipeline,
                view_width,
                view_height,
                fps_cap,
                ..
            } if got_ack => {
                break (
                    width,
                    height,
                    chroma,
                    scale_milli,
                    pipeline,
                    (view_width, view_height),
                    fps_cap,
                )
            }
            ServerMsg::Error { code, message } => {
                anyhow::bail!("server error {code}: {message}")
            }
            other => anyhow::bail!("unexpected message before StreamConfig: {other:?}"),
        }
    };
    let mut decoder = new_decoder(&gpu, chroma, width, height)?;
    let local = if gpu.is_some() { "gpu" } else { "cpu" };
    let mut video_label = format!("{} > {}", pipeline.label(), local);
    let _ = status.send(Status::Connected {
        video: video_label.clone(),
        view_width: view.0,
        view_height: view.1,
        fps_cap,
    });
    let decode_on = gpu.as_ref().map_or("cpu", |g| g.name.as_str());
    tracing::info!(width, height, scale_milli, decode_on, video = %video_label, "connected");
    let mut logged_first = false;

    // Writes run on their own task, fed by `out_tx`, so reads (draining video)
    // never block on a write and the two peers cannot deadlock. Both the reader
    // loop (acks, keyframe requests) and the UI thread (input) feed `out_tx`.
    let (out_tx, mut out_rx) = unbounded_channel::<(ClientMsg, Bytes)>();
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
    // Clipboard transfers write through the same channel, in chunks.
    let (clip_out_tx, mut clip_out_rx) = outbound_channel();
    let clipboard = Rc::new(Bridge::new(
        Transfers::new(clip_out_tx, Side::Client),
        status.clone(),
    ));
    {
        let out_tx = out_tx.clone();
        tokio::task::spawn_local(async move {
            while let Some((m, payload)) = clip_out_rx.recv().await {
                if out_tx.send((ClientMsg::from(m), payload)).is_err() {
                    break;
                }
            }
        });
    }
    // Forward UI input into the same write channel; clipboard commands from
    // the UI go to the bridge. The UI closes its end when it switches to
    // another machine; `ui_gone` then ends this session and, with it, the
    // ssh child. Keymap changes share the task and go first, so a key from
    // a newly used keyboard never reaches the server ahead of its keymap.
    let (ui_gone_tx, mut ui_gone) = tokio::sync::oneshot::channel::<()>();
    {
        let out_tx = out_tx.clone();
        let clipboard = clipboard.clone();
        tokio::task::spawn_local(async move {
            let mut follow_keymap = true;
            loop {
                let msg = tokio::select! {
                    biased;
                    changed = keymap.changed(), if follow_keymap => {
                        if changed.is_err() {
                            follow_keymap = false;
                            continue;
                        }
                        let text = keymap.borrow_and_update().clone();
                        tracing::debug!(bytes = text.len(), "sending changed keymap");
                        ClientMsg::Keymap { keymap: text }
                    }
                    cmd = input.recv() => match cmd {
                        Some(ToWorker::Send(m)) => m,
                        Some(other) => {
                            clipboard.on_ui(other);
                            continue;
                        }
                        None => break,
                    },
                };
                if out_tx.send((msg, Bytes::new())).is_err() {
                    return;
                }
            }
            let _ = ui_gone_tx.send(());
        });
    }

    let mut frames_since = 0u32;
    let mut bytes_since = 0u64;
    let mut decode_ms_acc = 0f32;
    let mut last_report = std::time::Instant::now();

    // The CPU decoder holds the newest picture of a High-profile
    // (GPU-encoded) stream until the next access unit arrives. When no video
    // frame has arrived for a while, drain it so the screen shows the latest
    // state. The deadline follows video frames only — cursor and other
    // messages must not postpone it — and a frame the full UI channel
    // rejected is retried at the same cadence.
    const IDLE_DRAIN: std::time::Duration = std::time::Duration::from_millis(150);
    let mut drain_at = tokio::time::Instant::now() + IDLE_DRAIN;
    let mut undelivered: Option<Picture> = None;
    // Geometry of the access units inside the decoder, oldest first. The CPU
    // decoder returns the PREVIOUS access unit's picture, and a scale-only
    // config can land in between: a picture must carry the geometry of the
    // config it was encoded under, not whatever is current when it emerges.
    let mut in_decoder: std::collections::VecDeque<Geometry> = std::collections::VecDeque::new();
    loop {
        let read = tokio::select! {
            read = reader.read_msg::<ServerMsg>() => read,
            _ = &mut ui_gone => return Ok(()),
            _ = tokio::time::sleep_until(drain_at),
                if undelivered.is_some()
                    || matches!(&decoder, VideoDecoder::Cpu(d) if d.has_pending()) =>
            {
                drain_at = tokio::time::Instant::now() + IDLE_DRAIN;
                let picture = match undelivered.take() {
                    Some(picture) => Some(picture),
                    None => match &mut decoder {
                        VideoDecoder::Cpu(d) => match d.flush() {
                            Ok(frame) => frame.map(|f| {
                                let (stream, view, scale_milli) = in_decoder
                                    .pop_front()
                                    .unwrap_or(((width, height), view, scale_milli));
                                Picture {
                                    frame: Frame::Bgra(f),
                                    stream,
                                    view,
                                    scale_milli,
                                }
                            }),
                            Err(e) => {
                                tracing::warn!(error = %e, "idle drain failed");
                                None
                            }
                        },
                        VideoDecoder::Gpu(_) => None,
                    },
                };
                if let Some(picture) = picture {
                    if let Err(std::sync::mpsc::TrySendError::Full(picture)) =
                        frames.try_send(picture)
                    {
                        undelivered = Some(picture);
                    }
                }
                continue;
            }
        };
        let msg = match read {
            Ok(m) => m,
            Err(gliff_transport::Error::Closed) => break,
            Err(e) => return Err(e.into()),
        };
        match msg {
            ServerMsg::VideoFrame {
                frame_id,
                keyframe: _,
                data_len,
                aux_len,
                ..
            } => {
                let main = reader.read_payload(data_len).await?;
                let aux = if aux_len > 0 {
                    reader.read_payload(aux_len).await?
                } else {
                    bytes::Bytes::new()
                };
                bytes_since += (data_len + aux_len) as u64;
                drain_at = tokio::time::Instant::now() + IDLE_DRAIN;
                in_decoder.push_back(((width, height), view, scale_milli));
                let t0 = std::time::Instant::now();
                let decoded = match &mut decoder {
                    VideoDecoder::Gpu(d) => d
                        .decode(&main, &aux)
                        .map(|f| f.map(Frame::Dmabuf))
                        .map_err(anyhow::Error::from),
                    VideoDecoder::Cpu(d) => d
                        .decode(&main, &aux)
                        .map(|f| f.map(Frame::Bgra))
                        .map_err(anyhow::Error::from),
                };
                let dec_ms = t0.elapsed().as_secs_f32() * 1000.0;
                // Ack immediately so the server keeps pacing.
                let _ = out_tx.send((
                    ClientMsg::FrameAck {
                        frame_id,
                        decoded_at_ms: now_ms(),
                    },
                    Bytes::new(),
                ));
                match decoded {
                    Ok(Some(frame)) => {
                        if !logged_first {
                            tracing::info!("first frame decoded");
                            logged_first = true;
                        }
                        // Latest-wins: a frame the full UI channel rejects
                        // waits in `undelivered` (superseding any drained
                        // one) and is retried at the drain cadence. The
                        // geometry travels with the frame, taken from the
                        // access unit that produced this picture (the GPU
                        // decoder returns the one just fed; the CPU decoder
                        // returns the previous one).
                        let (stream, pic_view, pic_scale) =
                            in_decoder
                                .pop_front()
                                .unwrap_or(((width, height), view, scale_milli));
                        let picture = Picture {
                            frame,
                            stream,
                            view: pic_view,
                            scale_milli: pic_scale,
                        };
                        match frames.try_send(picture) {
                            Ok(()) => undelivered = None,
                            Err(std::sync::mpsc::TrySendError::Full(picture)) => {
                                undelivered = Some(picture)
                            }
                            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {}
                        }
                        frames_since += 1;
                        decode_ms_acc += dec_ms;
                    }
                    Ok(None) => {
                        // The GPU decoder never buffers across calls: no
                        // picture means this access unit produced none, so
                        // its geometry entry goes with it. The CPU decoder
                        // holds it (drained later or by the next decode).
                        if matches!(decoder, VideoDecoder::Gpu(_)) {
                            in_decoder.pop_front();
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "decode error; requesting keyframe");
                        in_decoder.clear();
                        let _ = out_tx.send((ClientMsg::RequestKeyframe, Bytes::new()));
                    }
                }
            }
            ServerMsg::StreamConfig {
                width: w,
                height: h,
                chroma: c,
                scale_milli: s,
                pipeline,
                view_width,
                view_height,
                fps_cap: f,
                ..
            } => {
                // A pace-only change (fps cap) must not reset the decoder;
                // a coded-stream change does, and drops the frame waiting
                // for the UI with it.
                if (w, h, c) != (width, height, chroma) {
                    decoder = new_decoder(&gpu, c, w, h)?;
                    undelivered = None;
                    in_decoder.clear();
                }
                (width, height, chroma, scale_milli) = (w, h, c, s);
                (view, fps_cap) = ((view_width, view_height), f);
                video_label = format!("{} > {}", pipeline.label(), local);
                let _ = status.send(Status::Connected {
                    video: video_label.clone(),
                    view_width,
                    view_height,
                    fps_cap,
                });
            }
            ServerMsg::CursorShape {
                width,
                height,
                hot_x,
                hot_y,
                argb_len,
                ..
            } => {
                let argb = reader.read_payload(argb_len).await?.to_vec();
                let _ = status.send(Status::Cursor {
                    width,
                    height,
                    hot_x,
                    hot_y,
                    argb,
                });
            }
            ServerMsg::Ping { t } => {
                let _ = out_tx.send((ClientMsg::Pong { t }, Bytes::new()));
            }
            ServerMsg::CursorPos { .. } | ServerMsg::Pong { .. } => {}
            ServerMsg::Error { code, message } => anyhow::bail!("server error {code}: {message}"),
            ServerMsg::HelloAck { .. } => {}
            other => {
                if let Ok(clip) = other.into_clipboard() {
                    let payload = match &clip {
                        ClipboardMsg::Data { data_len, .. } => {
                            if *data_len as usize > CHUNK {
                                anyhow::bail!(
                                    "clipboard chunk of {data_len} bytes exceeds the limit"
                                );
                            }
                            reader.read_payload(*data_len).await?
                        }
                        _ => Bytes::new(),
                    };
                    clipboard.on_peer_msg(clip, payload);
                }
            }
        }
        if last_report.elapsed().as_secs_f32() >= 1.0 {
            let secs = last_report.elapsed().as_secs_f32();
            let _ = status.send(Status::Stats {
                fps: frames_since as f32 / secs,
                mbit: bytes_since as f32 * 8.0 / 1_000_000.0 / secs,
                decode_ms: if frames_since > 0 {
                    decode_ms_acc / frames_since as f32
                } else {
                    0.0
                },
                video: video_label.clone(),
            });
            frames_since = 0;
            bytes_since = 0;
            decode_ms_acc = 0.0;
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
