//! One client session: output setup, the capture/encode/send loop, and input.

use std::collections::VecDeque;
use std::ops::ControlFlow;
use std::os::fd::AsFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use gliff_proto::{
    ChromaMode, ClientCaps, ClientMsg, Codec, OutputInfo as ProtoOutput, Rect, ServerMsg,
    SessionInfo, PROTOCOL_VERSION,
};
use gliff_transport::Framed;
use gliff_vk::{DmabufPlane, Encoder, EncoderSettings, Gpu};
use hypr_capture::{CaptureConfig, CaptureEvent, CapturedFrame, Capturer};
use hypr_input::{Axis as InAxis, Clipboard, ClipboardEvent, Input, InputCmd, InputConfig};
use hypr_wl::Target;

pub struct Config {
    pub target: Target,
    pub output: Option<String>,
    pub render_node: PathBuf,
    pub low_bandwidth: bool,
    pub bitrate: Option<u32>,
    /// Frames per second the stream is paced at, at most.
    pub max_fps: u32,
}

const TEXT_MIME: &str = "text/plain;charset=utf-8";

/// Events from the capture thread.
enum Incoming {
    Frame(CapturedFrame),
    Cursor {
        width: u32,
        height: u32,
        hot_x: i32,
        hot_y: i32,
        argb: Vec<u8>,
    },
    CursorPos {
        x: f64,
        y: f64,
        visible: bool,
    },
    Stopped,
    Error(String),
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub async fn run<R, W>(rd: R, wr: W, cfg: Config) -> Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + 'static,
{
    let mut reader = Framed::new(rd);
    let mut writer = Framed::new(wr);

    let (keymap, caps) = handshake(&mut reader, &mut writer).await?;

    let instance = cfg.target.instance().context("find Hyprland instance")?;
    let output = setup_output(&instance, &cfg, &caps)?;
    tracing::info!(output = %output.name, output.width, output.height, headless = output.is_headless(), "session output ready");

    let chroma = if cfg.low_bandwidth || !caps.chroma.contains(&ChromaMode::Dual420) {
        ChromaMode::Single420
    } else {
        ChromaMode::Dual420
    };
    let codec = Codec::H264;
    writer
        .write_msg(&ServerMsg::HelloAck {
            version: PROTOCOL_VERSION,
            session: SessionInfo {
                headless: output.is_headless(),
                output: output.name.clone(),
            },
            outputs: vec![ProtoOutput {
                name: output.name.clone(),
                width: output.width,
                height: output.height,
                scale_milli: 1000,
            }],
        })
        .await?;
    let stream = (output.width, output.height);
    writer
        .write_msg(&stream_config(codec, chroma, &output, stream))
        .await?;

    let (cap_tx, mut cap_rx) = mpsc::unbounded_channel();
    let capturer = start_capture(&cfg.target, &output.name, &cfg.render_node, cap_tx)?;

    let input = Arc::new(start_input(&cfg.target, &output.name, &keymap)?);
    input.send(output.logical_extent()).ok();

    let (clip_out_tx, mut clip_out_rx) = mpsc::unbounded_channel::<String>();
    let clipboard = Clipboard::start(
        cfg.target.clone(),
        Box::new(move |ClipboardEvent::Text(t)| {
            let _ = clip_out_tx.send(t);
        }),
    )
    .map_err(|e| tracing::warn!(error = %e, "clipboard bridge unavailable"))
    .ok();

    let gpu = Gpu::open(Some(&cfg.render_node)).context("open Vulkan device")?;
    let streams = if chroma == ChromaMode::Dual420 { 2 } else { 1 };
    let bitrate_ctl = BitrateController::new(stream.0, stream.1, streams, cfg.bitrate);
    let max_fps = cfg.max_fps.clamp(MIN_RC_FPS, 240);
    let settings = encoder_settings(stream.0, stream.1, bitrate_ctl.bitrate(max_fps), max_fps);
    let encoder = Encoder::new(&gpu, settings.clone(), chroma == ChromaMode::Dual420)
        .context("create encoder")?;

    let (mut msg_rx, mut clip_in_rx) = spawn_reader(reader, input.clone());
    let (out, queued_frames) = spawn_writer(writer);

    capturer.request_frame().ok();
    let mut session = Session {
        out,
        queued_frames,
        gpu,
        instance,
        output,
        stream,
        caps,
        bitrate_ctl,
        codec,
        chroma,
        settings,
        encoder,
        input,
        capturer,
        resize_request: None,
        pending: None,
        capture_asked: true,
        blocked_noted: false,
        in_flight: 0,
        n_limit: 2,
        frame_id: 0,
        want_keyframe: true,
        rtt: RttEstimator::new(),
        cursor_shape_id: 0,
        encode_us: 0.0,
        max_fps,
        next_send_at: Instant::now(),
    };

    loop {
        let pace = tokio::time::Instant::from_std(session.next_send_at);
        let flow = tokio::select! {
            _ = tokio::time::sleep_until(pace), if session.pending.is_some() => ControlFlow::Continue(()),
            msg = msg_rx.recv() => match msg {
                Some(msg) => session.on_client_msg(msg)?,
                None => {
                    tracing::info!("client disconnected");
                    ControlFlow::Break(())
                }
            },
            text = clip_out_rx.recv() => {
                if let Some(text) = text {
                    session.send_clipboard(text)?;
                }
                ControlFlow::Continue(())
            }
            text = clip_in_rx.recv() => {
                if let (Some(text), Some(clip)) = (text, &clipboard) {
                    clip.set_text(text);
                }
                ControlFlow::Continue(())
            }
            ev = cap_rx.recv() => session.on_capture(ev)?,
        };
        if flow.is_break() {
            break;
        }
        if let Some((w, h, scale)) = session.resize_request.take() {
            session.resize(w, h, scale).await?;
        }
        session.pump_encoder().await?;
    }

    session.inject(InputCmd::ReleaseAll);
    // Dropping the session removes a created headless output.
    Ok(())
}

/// Read the client's Hello and check its protocol version.
async fn handshake<R, W>(
    reader: &mut Framed<R>,
    writer: &mut Framed<W>,
) -> Result<(String, ClientCaps)>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    match reader.read_msg::<ClientMsg>().await.context("read Hello")? {
        ClientMsg::Hello {
            version,
            keymap,
            caps,
        } => {
            if version != PROTOCOL_VERSION {
                writer
                    .write_msg(&ServerMsg::Error {
                        code: 1,
                        message: format!("version {version} unsupported"),
                    })
                    .await?;
                anyhow::bail!("client version {version} != {PROTOCOL_VERSION}");
            }
            Ok((keymap, caps))
        }
        other => anyhow::bail!("expected Hello, got {other:?}"),
    }
}

/// Drain the socket on its own thread. Input is injected from there, so a
/// key or pointer event never waits behind an encode or a blocked write on
/// the session thread; everything else is handed over on a channel. Clipboard
/// payloads are consumed inline and delivered as text.
fn spawn_reader<R>(
    mut reader: Framed<R>,
    input: Arc<Input>,
) -> (UnboundedReceiver<ClientMsg>, UnboundedReceiver<String>)
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let (msg_tx, msg_rx) = mpsc::unbounded_channel();
    let (clip_tx, clip_rx) = mpsc::unbounded_channel();
    std::thread::Builder::new()
        .name("gliff-reader".into())
        .spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            rt.block_on(async move {
                loop {
                    match reader.read_msg::<ClientMsg>().await {
                        Ok(ClientMsg::ClipboardData { data_len, .. }) => {
                            if data_len as u64 > gliff_proto::CLIPBOARD_MAX {
                                break;
                            }
                            match reader.read_payload(data_len).await {
                                Ok(bytes) => {
                                    if let Ok(text) = String::from_utf8(bytes.to_vec()) {
                                        let _ = clip_tx.send(text);
                                    }
                                }
                                Err(_) => break,
                            }
                        }
                        Ok(msg) => {
                            if let Some(cmd) = input_cmd(&msg) {
                                if input.send(cmd).is_err() {
                                    break;
                                }
                            } else if msg_tx.send(msg).is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
            });
        })
        .expect("spawn reader thread");
    (msg_rx, clip_rx)
}

/// The input injection a client message asks for, if it is one.
fn input_cmd(msg: &ClientMsg) -> Option<InputCmd> {
    Some(match *msg {
        ClientMsg::Key { keycode, pressed } => InputCmd::Key {
            code: keycode,
            pressed,
        },
        ClientMsg::PointerMotion { x, y } => InputCmd::Motion { x, y },
        ClientMsg::PointerButton { button, pressed } => InputCmd::Button { button, pressed },
        ClientMsg::PointerAxis {
            axis,
            value,
            discrete,
            stop,
        } => InputCmd::Axis {
            axis: match axis {
                gliff_proto::Axis::Vertical => InAxis::Vertical,
                gliff_proto::Axis::Horizontal => InAxis::Horizontal,
            },
            value,
            discrete,
            stop,
        },
        _ => return None,
    })
}

/// A message and the payloads that follow it on the wire.
struct Outgoing {
    msg: ServerMsg,
    payloads: Vec<Vec<u8>>,
    video: bool,
}

/// Write on a separate task so a write that blocks on a full link stalls
/// neither acks nor capture. The counter is the video frames handed over and
/// not yet fully written, so the session can see the link is behind.
fn spawn_writer<W>(mut writer: Framed<W>) -> (UnboundedSender<Outgoing>, Arc<AtomicU32>)
where
    W: AsyncWrite + Unpin + 'static,
{
    let (tx, mut rx) = mpsc::unbounded_channel::<Outgoing>();
    let queued = Arc::new(AtomicU32::new(0));
    let counter = queued.clone();
    tokio::task::spawn_local(async move {
        while let Some(out) = rx.recv().await {
            let payloads: Vec<&[u8]> = out.payloads.iter().map(|p| p.as_slice()).collect();
            let r = writer.write_msg_with_payloads(&out.msg, &payloads).await;
            if out.video {
                counter.fetch_sub(1, Ordering::AcqRel);
            }
            if let Err(e) = r {
                tracing::info!(error = %e, "write failed; closing");
                break;
            }
        }
    });
    (tx, queued)
}

struct Session {
    out: UnboundedSender<Outgoing>,
    /// Video frames handed to the writer and not yet on the wire.
    queued_frames: Arc<AtomicU32>,
    gpu: Arc<Gpu>,
    instance: hypr_ipc::Instance,
    output: SessionOutput,
    /// Encoded size. Equals the output size, except for a mirrored screen
    /// larger than the client's window, which is scaled down to fit.
    stream: (u32, u32),
    caps: ClientCaps,
    bitrate_ctl: BitrateController,
    codec: Codec,
    chroma: ChromaMode,
    settings: EncoderSettings,
    encoder: Encoder,
    input: Arc<Input>,
    capturer: Capturer,
    /// A Resize from the client, applied on the session loop.
    resize_request: Option<(u32, u32, f32)>,
    /// The newest captured frame not yet encoded.
    pending: Option<CapturedFrame>,
    capture_asked: bool,
    /// The pending frame has already counted as blocked for the bitrate
    /// controller.
    blocked_noted: bool,
    /// Frames sent but not yet acked; bounded by `n_limit` for pacing.
    in_flight: u32,
    n_limit: u32,
    frame_id: u64,
    want_keyframe: bool,
    rtt: RttEstimator,
    cursor_shape_id: u32,
    /// Smoothed time to split and encode one frame, in microseconds.
    encode_us: f64,
    max_fps: u32,
    /// The pace: no frame is encoded before this instant.
    next_send_at: Instant,
}

/// Slowest pace the rate control is told about; below this each frame would
/// take too long to send.
const MIN_RC_FPS: u32 = 5;

impl Session {
    /// Queue a message for the writer. A closed writer ends the session.
    fn send(&self, msg: ServerMsg, payloads: Vec<Vec<u8>>, video: bool) -> Result<()> {
        if video {
            self.queued_frames.fetch_add(1, Ordering::AcqRel);
        }
        self.out
            .send(Outgoing {
                msg,
                payloads,
                video,
            })
            .map_err(|_| anyhow::anyhow!("connection closed"))
    }

    fn on_client_msg(&mut self, msg: ClientMsg) -> Result<ControlFlow<()>> {
        match msg {
            ClientMsg::Bye => return Ok(ControlFlow::Break(())),
            ClientMsg::FrameAck {
                frame_id,
                decoded_at_ms,
            } => {
                self.in_flight = self.in_flight.saturating_sub(1);
                let acked = self.rtt.record(frame_id, decoded_at_ms);
                self.n_limit = self.rtt.window(self.settings.framerate);
                if self.bitrate_ctl.on_ack(acked, &mut self.rtt, self.settings.framerate) {
                    self.apply_rate();
                }
            }
            ClientMsg::RequestKeyframe => self.want_keyframe = true,
            // Input is injected by the reader thread.
            ClientMsg::Key { .. }
            | ClientMsg::PointerMotion { .. }
            | ClientMsg::PointerButton { .. }
            | ClientMsg::PointerAxis { .. } => {}
            ClientMsg::Resize {
                width,
                height,
                scale,
            } => {
                self.resize_request = Some((width, height, scale));
            }
            ClientMsg::Ping { t } => self.send(
                ServerMsg::Pong {
                    t,
                    server_now_ms: now_ms(),
                },
                Vec::new(),
                false,
            )?,
            // ClipboardData is consumed by the reader thread; the
            // offer/request negotiation is not used for text.
            ClientMsg::ClipboardData { .. }
            | ClientMsg::ClipboardOffer { .. }
            | ClientMsg::ClipboardRequest { .. } => {}
            ClientMsg::Hello { .. } => anyhow::bail!("unexpected second Hello"),
        }
        Ok(ControlFlow::Continue(()))
    }

    /// Track how long frames take to encode and tell the rate control the
    /// pace that gives, so the per-frame budget matches the bandwidth: an
    /// encoder that sustains 25 fps at 4K should not spend a 60 fps budget.
    fn note_encode_time(&mut self, enc_us: u128) {
        let sample = enc_us as f64;
        self.encode_us = if self.encode_us == 0.0 {
            sample
        } else {
            0.8 * self.encode_us + 0.2 * sample
        };
        let sustainable = (1_000_000.0 / (self.encode_us * 1.1).max(1.0)) as u32;
        let fps = sustainable.clamp(MIN_RC_FPS, self.max_fps);
        let current = self.settings.framerate;
        if (fps as f64 - current as f64).abs() / current as f64 > 0.15 {
            self.settings.framerate = fps;
            self.apply_rate();
        }
    }

    /// Program the encoder with the budget at the current pace.
    fn apply_rate(&mut self) {
        let fps = self.settings.framerate;
        self.settings.bitrate = self.bitrate_ctl.bitrate(fps);
        self.encoder.set_rate(self.settings.bitrate, fps);
    }

    /// Forward an input event; a dead input thread ends the session elsewhere.
    fn inject(&self, cmd: InputCmd) {
        let _ = self.input.send(cmd);
    }

    /// Resize a headless output to the client's window, within the size it
    /// declared in Hello, and restart the encoder at the new size.
    /// Resize a headless output to the client's window at the client's
    /// scale, so the remote UI renders at the client's DPI.
    async fn resize(&mut self, width: u32, height: u32, scale: f32) -> Result<()> {
        let width = width.min(self.caps.max_width) & !1;
        let height = height.min(self.caps.max_height) & !1;
        let scale = if scale.is_finite() {
            scale.clamp(0.5, 4.0)
        } else {
            1.0
        };
        if width < 320 || height < 240 {
            return Ok(());
        }
        if !self.output.is_headless() {
            return self.fit_mirror(width, height).await;
        }
        let same_size = (width, height) == (self.output.width, self.output.height);
        let same_scale = (scale - self.output.scale).abs() < 0.01;
        if same_size && same_scale {
            return Ok(());
        }
        tracing::info!(width, height, scale, "resizing headless output");
        self.instance
            .set_monitor_mode(&self.output.name, width, height, 60, scale)
            .ok();
        // Hyprland applies the mode asynchronously and may round the scale
        // so the logical size is whole; wait for it and use what it chose.
        let applied = self.wait_for_mode(width, height, scale).await;
        if !same_size {
            self.bitrate_ctl = BitrateController::new(width, height, self.bitrate_ctl.streams, self.bitrate_ctl.cap);
            let settings = encoder_settings(width, height, self.bitrate_ctl.bitrate(self.max_fps), self.max_fps);
            match Encoder::new(
                &self.gpu,
                settings.clone(),
                self.chroma == ChromaMode::Dual420,
            ) {
                Ok(encoder) => {
                    self.encoder = encoder;
                    self.settings = settings;
                    self.encode_us = 0.0;
                    self.output.width = width;
                    self.output.height = height;
                    self.stream = (width, height);
                    self.pending = None;
                    self.want_keyframe = true;
                }
                Err(e) => {
                    // Keep streaming at the old size rather than end the session.
                    tracing::warn!(error = %e, width, height, "encoder rejected the new size; keeping the old one");
                    let (w, h, s) = (self.output.width, self.output.height, self.output.scale);
                    self.instance
                        .set_monitor_mode(&self.output.name, w, h, 60, s)
                        .ok();
                    self.wait_for_mode(w, h, s).await;
                    self.inject(self.output.logical_extent());
                    return Ok(());
                }
            }
        }
        self.output.scale = applied;
        self.inject(self.output.logical_extent());
        self.send(
            stream_config(self.codec, self.chroma, &self.output, self.stream),
            Vec::new(),
            false,
        )
    }

    /// A mirrored screen keeps its size; when it is larger than the client's
    /// window the stream is scaled down to fit (never up), so the link and
    /// the decoder carry only what the window can show.
    async fn fit_mirror(&mut self, win_w: u32, win_h: u32) -> Result<()> {
        let (ow, oh) = (self.output.width as f64, self.output.height as f64);
        let fit = (win_w as f64 / ow).min(win_h as f64 / oh).min(1.0);
        let stream = (
            ((ow * fit).round() as u32).max(2) & !1,
            ((oh * fit).round() as u32).max(2) & !1,
        );
        if stream == self.stream {
            return Ok(());
        }
        self.bitrate_ctl = BitrateController::new(stream.0, stream.1, self.bitrate_ctl.streams, self.bitrate_ctl.cap);
        let settings = encoder_settings(stream.0, stream.1, self.bitrate_ctl.bitrate(self.max_fps), self.max_fps);
        match Encoder::new(
            &self.gpu,
            settings.clone(),
            self.chroma == ChromaMode::Dual420,
        ) {
            Ok(encoder) => {
                tracing::info!(
                    width = stream.0,
                    height = stream.1,
                    "scaling the mirrored screen to the window"
                );
                self.encoder = encoder;
                self.settings = settings;
                self.encode_us = 0.0;
                self.stream = stream;
                self.pending = None;
                self.want_keyframe = true;
                self.send(
                    stream_config(self.codec, self.chroma, &self.output, self.stream),
                    Vec::new(),
                    false,
                )
            }
            Err(e) => {
                tracing::warn!(error = %e, "encoder rejected the fitted size; keeping the current one");
                Ok(())
            }
        }
    }

    /// Poll until the output reports the requested mode (Hyprland applies it
    /// asynchronously and may round the scale), for up to half a second,
    /// without blocking the session. Returns the scale in effect.
    async fn wait_for_mode(&self, width: u32, height: u32, scale: f32) -> f32 {
        let mut seen = None;
        for _ in 0..10 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let Ok(mons) = self.instance.monitors() else {
                continue;
            };
            if let Some(m) = mons
                .iter()
                .find(|m| m.name == self.output.name && m.width == width && m.height == height)
            {
                seen = Some(m.scale);
                if (m.scale - scale).abs() < 0.15 {
                    return m.scale;
                }
            }
        }
        seen.unwrap_or(scale)
    }

    fn send_clipboard(&mut self, text: String) -> Result<()> {
        let bytes = text.into_bytes();
        let total = bytes.len() as u64;
        let msg = ServerMsg::ClipboardData {
            mime_type: TEXT_MIME.into(),
            offset: 0,
            total,
            data_len: bytes.len() as u32,
        };
        self.send(msg, vec![bytes], false)
    }

    fn on_capture(&mut self, ev: Option<Incoming>) -> Result<ControlFlow<()>> {
        match ev {
            Some(Incoming::Frame(image)) => {
                self.pending = Some(image);
                self.capture_asked = false;
                self.blocked_noted = false;
            }
            Some(Incoming::Cursor {
                width,
                height,
                hot_x,
                hot_y,
                argb,
            }) => {
                self.cursor_shape_id += 1;
                let msg = ServerMsg::CursorShape {
                    id: self.cursor_shape_id,
                    width,
                    height,
                    hot_x,
                    hot_y,
                    argb_len: argb.len() as u32,
                };
                self.send(msg, vec![argb], false)?;
            }
            Some(Incoming::CursorPos { x, y, visible }) => {
                self.send(
                    ServerMsg::CursorPos {
                        x,
                        y,
                        shape_id: self.cursor_shape_id,
                        visible,
                    },
                    Vec::new(),
                    false,
                )?;
            }
            Some(Incoming::Stopped) => {
                tracing::warn!("capture stopped");
                return Ok(ControlFlow::Break(()));
            }
            Some(Incoming::Error(e)) => {
                tracing::error!(error = %e, "capture error");
                return Ok(ControlFlow::Break(()));
            }
            None => return Ok(ControlFlow::Break(())),
        }
        Ok(ControlFlow::Continue(()))
    }

    /// The client has ack capacity and the previous frame has left for the
    /// wire, so encoding another now will not build a queue.
    fn can_send(&self) -> bool {
        self.link_has_room() && Instant::now() >= self.next_send_at
    }

    /// The link side of `can_send`: waiting for a pace slot is not
    /// congestion, so only this part counts as a blocked frame.
    fn link_has_room(&self) -> bool {
        self.in_flight < self.n_limit && self.queued_frames.load(Ordering::Acquire) == 0
    }

    /// Encode and send the pending frame if the link can take it, then ask
    /// the capture thread for the next one. While the link cannot take a
    /// frame, capture continues so the frame that eventually goes out is the
    /// newest, not the one that was waiting.
    async fn pump_encoder(&mut self) -> Result<()> {
        // Count a blocked frame once, not once per event-loop pass.
        if self.pending.is_some() && !self.link_has_room() && !self.blocked_noted {
            self.bitrate_ctl.note_blocked();
            self.blocked_noted = true;
        }
        if self.can_send() {
            if let Some(frame) = self.pending.take() {
                // Ask for the next frame before this one encodes, so the
                // compositor prepares it while the GPU is busy.
                self.request_capture();
                // A frame captured before a resize took effect is stale.
                let info = &frame.buffer.info;
                if (info.width & !1, info.height & !1) == (self.output.width, self.output.height) {
                    self.encode_and_send(&frame).await?;
                }
            }
        }
        self.request_capture();
        Ok(())
    }

    fn request_capture(&mut self) {
        if !self.capture_asked {
            self.capturer.request_frame().ok();
            self.capture_asked = true;
        }
    }

    async fn encode_and_send(&mut self, frame: &CapturedFrame) -> Result<()> {
        let (width, height) = self.stream;
        let info = &frame.buffer.info;
        let fourcc = drm_fourcc::DrmFourcc::try_from(info.fourcc).map_err(|_| {
            anyhow::anyhow!("capture fourcc {:#x} is not a DRM format", info.fourcc)
        })?;
        let plane = DmabufPlane {
            fd: info.fd.as_fd(),
            width: info.width,
            height: info.height,
            offset: info.planes[0].offset,
            stride: info.planes[0].stride,
            fourcc,
            modifier: info.modifier,
        };
        // The import is cached per ring buffer; the generation changes when
        // the ring is reallocated (resize), so old imports are never reused.
        let buffer_key = (frame.buffer.generation << 32) | frame.buffer.index as u64;
        let key = std::mem::take(&mut self.want_keyframe);
        // The pace counts from the start of the encode, so an encode that
        // takes longer than the interval never adds a wait of its own. When
        // we keep up, the next slot follows this one, so the cadence is even.
        let interval = Duration::from_secs_f64(1.0 / self.max_fps as f64);
        let t0 = Instant::now();
        let from_slot = self.next_send_at + interval;
        self.next_send_at = if from_slot > t0 { from_slot } else { t0 + interval };
        let encoded = self.encoder.encode_dmabuf(buffer_key, &plane, key)?;
        let enc_us = t0.elapsed().as_micros();
        let aux = encoded.aux.unwrap_or_default();
        let msg = ServerMsg::VideoFrame {
            frame_id: self.frame_id,
            pts_us: now_ms() * 1000,
            keyframe: encoded.keyframe,
            damage: vec![Rect {
                x: 0,
                y: 0,
                width: width as i32,
                height: height as i32,
            }],
            data_len: encoded.main.len() as u32,
            aux_len: aux.len() as u32,
        };
        let bytes = encoded.main.len() + aux.len();
        self.send(msg, vec![encoded.main, aux], true)?;
        tracing::debug!(
            frame_id = self.frame_id,
            key = encoded.keyframe,
            bytes,
            enc_us,
            in_flight = self.in_flight,
            n_limit = self.n_limit,
            "sent frame"
        );
        self.rtt.on_sent(self.frame_id, bytes);
        self.bitrate_ctl.note_sent();
        self.note_encode_time(enc_us);
        self.frame_id += 1;
        self.in_flight += 1;
        Ok(())
    }
}

/// The output being served. A created headless output is removed on drop, on
/// every exit path including a failure later in setup.
struct SessionOutput {
    name: String,
    /// Physical (captured) size.
    width: u32,
    height: u32,
    /// Output scale; the logical size the pointer works in is size / scale.
    scale: f32,
    headless: Option<hypr_ipc::Instance>,
}

impl SessionOutput {
    fn is_headless(&self) -> bool {
        self.headless.is_some()
    }

    /// The logical extent Hyprland exposes to clients and the virtual pointer.
    fn logical_extent(&self) -> InputCmd {
        let s = self.scale.max(0.5);
        InputCmd::SetExtent {
            width: (self.width as f32 / s).round().max(1.0) as u32,
            height: (self.height as f32 / s).round().max(1.0) as u32,
        }
    }
}

impl Drop for SessionOutput {
    fn drop(&mut self) {
        if let Some(instance) = &self.headless {
            let _ = instance.remove_output(&self.name);
        }
    }
}

fn stream_config(
    codec: Codec,
    chroma: ChromaMode,
    output: &SessionOutput,
    stream: (u32, u32),
) -> ServerMsg {
    // The scale the client divides stream pixels by to reach the remote's
    // logical space: the output scale times any downscale of the stream.
    let effective_scale = output.scale * stream.0 as f32 / output.width.max(1) as f32;
    // Parameter sets ride in-band on every keyframe, so extradata is empty.
    ServerMsg::StreamConfig {
        codec,
        chroma,
        width: stream.0,
        height: stream.1,
        scale_milli: (effective_scale * 1000.0).round() as u32,
        extradata: Vec::new(),
        aux_extradata: None,
    }
}

fn encoder_settings(width: u32, height: u32, bitrate: u32, framerate: u32) -> EncoderSettings {
    EncoderSettings {
        width,
        height,
        bitrate,
        framerate,
    }
}

/// Pick the named output, or create a headless one sized to the client.
fn setup_output(
    instance: &hypr_ipc::Instance,
    cfg: &Config,
    caps: &ClientCaps,
) -> Result<SessionOutput> {
    if let Some(name) = &cfg.output {
        let mons = instance.monitors()?;
        let m = if name == "auto" {
            main_monitor(&mons)
        } else {
            mons.iter().find(|m| &m.name == name)
        }
        .with_context(|| format!("no output {name}"))?;
        return Ok(SessionOutput {
            name: m.name.clone(),
            width: m.width & !1,
            height: m.height & !1,
            scale: m.scale,
            headless: None,
        });
    }
    let before: Vec<String> = instance.monitors()?.into_iter().map(|m| m.name).collect();
    let requested = format!("gliff-{}", std::process::id());
    instance
        .create_headless_output(&requested)
        .context("create headless output")?;
    // Hyprland names the output itself, so find the one that appeared.
    let mut output = SessionOutput {
        name: requested,
        width: 0,
        height: 0,
        scale: 1.0,
        headless: Some(instance.clone()),
    };
    std::thread::sleep(Duration::from_millis(200));
    let after = instance.monitors()?;
    let m = after
        .iter()
        .find(|m| !before.contains(&m.name))
        .context("headless output did not appear")?;
    output.name = m.name.clone();
    output.width = caps.max_width.clamp(320, 1920) & !1;
    output.height = caps.max_height.clamp(240, 1080) & !1;
    instance
        .set_monitor_mode(&output.name, output.width, output.height, 60, 1.0)
        .ok();
    std::thread::sleep(Duration::from_millis(150));
    Ok(output)
}

/// The screen the user is most likely looking at: the focused monitor, else
/// the leftmost enabled one.
fn main_monitor(mons: &[hypr_ipc::Monitor]) -> Option<&hypr_ipc::Monitor> {
    let enabled = || mons.iter().filter(|m| !m.disabled);
    enabled()
        .find(|m| m.focused)
        .or_else(|| enabled().min_by_key(|m| (m.x, m.y)))
}

fn start_capture(
    target: &Target,
    output: &str,
    render_node: &std::path::Path,
    tx: UnboundedSender<Incoming>,
) -> Result<Capturer> {
    let mut cc = CaptureConfig::new(output.to_string());
    cc.target = target.clone();
    cc.render_node = render_node.to_path_buf();
    cc.cursor = true;
    let sink = Box::new(move |ev: CaptureEvent| {
        let msg = match ev {
            CaptureEvent::Frame(frame) => Incoming::Frame(frame),
            CaptureEvent::CursorShape {
                width,
                height,
                hot_x,
                hot_y,
                argb,
            } => Incoming::Cursor {
                width,
                height,
                hot_x,
                hot_y,
                argb,
            },
            CaptureEvent::CursorPos { x, y, visible } => Incoming::CursorPos {
                x: x as f64,
                y: y as f64,
                visible,
            },
            CaptureEvent::Stopped => Incoming::Stopped,
            CaptureEvent::Error(e) => Incoming::Error(e),
            CaptureEvent::Ready { .. } => return,
        };
        let _ = tx.send(msg);
    });
    Ok(Capturer::start(cc, sink)?)
}

fn start_input(target: &Target, output: &str, keymap: &str) -> Result<Input> {
    let mut ic = InputConfig::new(output.to_string());
    ic.target = target.clone();
    ic.keymap = (!keymap.is_empty()).then(|| keymap.to_string());
    Ok(Input::start(ic, Box::new(|_| {}))?)
}

/// Ack round-trip time. The smoothed value tracks the path as it is; the
/// base (its minimum) is the path without queueing. The difference is how
/// much has queued up in front of the client.
struct RttEstimator {
    sent: VecDeque<(u64, Instant, usize)>,
    smoothed_ms: f64,
    base_ms: f64,
    /// Lowest sample since the controller last looked. A queue raises every
    /// sample; decode jitter raises only some, so the minimum tells the two
    /// apart.
    window_min_ms: f64,
}

/// Adapts the per-frame bit budget to the path. The encoder is CBR at
/// `budget x pace`, so the budget is what each frame may cost and the pace
/// converts it to a bandwidth. Growth of the ack RTT over its base is
/// queueing; the rate the acks then arrive at is what the link delivers, so
/// the budget drops to a fraction of that in one step. A quiet path grows
/// the budget back: quickly at first (slow start), then gently.
struct BitrateController {
    /// Bits per frame per stream, bounded by the quality ceiling and floor.
    min: u32,
    max: u32,
    current: u32,
    /// Optional cap on total bits per second across the streams.
    cap: Option<u32>,
    streams: u32,
    last_eval: Instant,
    last_change: Instant,
    sent: u32,
    blocked: u32,
    /// Acks in the current evaluation window: arrival time and the bytes
    /// of the frame each covers.
    acks: Vec<(Instant, usize)>,
    /// Growth steps are large until the first cut.
    slow_start: bool,
}

impl BitrateController {
    const EVAL_EVERY: Duration = Duration::from_millis(250);
    const GROW_AFTER: Duration = Duration::from_millis(1500);
    const QUEUE_HIGH_MS: f64 = 50.0;
    const QUEUE_LOW_MS: f64 = 10.0;
    /// Bits per pixel per stream: the quality ceiling, and the floor below
    /// which a frame is not worth sending.
    const MAX_BPP: f64 = 0.1;
    const MIN_BPP: f64 = 0.005;

    fn new(width: u32, height: u32, streams: u32, cap: Option<u32>) -> Self {
        let pixels = width as f64 * height as f64;
        let max = (pixels * Self::MAX_BPP) as u32;
        let min = (pixels * Self::MIN_BPP) as u32;
        let now = Instant::now();
        Self {
            min,
            max,
            current: (max / 4).max(min),
            cap,
            streams: streams.max(1),
            last_eval: now,
            last_change: now,
            sent: 0,
            blocked: 0,
            acks: Vec::new(),
            slow_start: true,
        }
    }

    /// The CBR bitrate for one stream at `pace` frames per second.
    fn bitrate(&self, pace: u32) -> u32 {
        let by_budget = self.current as u64 * pace as u64;
        let by_cap = self
            .cap
            .map(|c| c as u64 / self.streams as u64)
            .unwrap_or(u64::MAX);
        by_budget.min(by_cap).max(1) as u32
    }

    fn note_sent(&mut self) {
        self.sent += 1;
    }

    /// A frame is waiting because every allowed frame is still unacked.
    fn note_blocked(&mut self) {
        self.blocked += 1;
    }

    /// Bits per second the acks in the window arrived at. When the link is
    /// saturated the acks are clocked by its transmission rate, so this is
    /// close to the link capacity.
    fn acked_rate(&self) -> Option<f64> {
        let (first, rest) = self.acks.split_first()?;
        if rest.len() < 2 {
            return None;
        }
        let span = rest[rest.len() - 1].0.duration_since(first.0).as_secs_f64();
        if span < 0.05 {
            return None;
        }
        let bytes: usize = rest.iter().map(|(_, b)| b).sum();
        Some(bytes as f64 * 8.0 / span)
    }

    /// Feed an ack: the bytes it covers, the RTT estimate and the current
    /// pace. Returns true when the budget changed.
    fn on_ack(&mut self, acked_bytes: usize, rtt: &mut RttEstimator, pace: u32) -> bool {
        let now = Instant::now();
        self.acks.push((now, acked_bytes));
        if now.duration_since(self.last_eval) < Self::EVAL_EVERY {
            return false;
        }
        self.last_eval = now;
        let queueing = rtt.take_queueing_ms();
        let starved = self.sent > 0 && self.blocked > self.sent;
        let (sent, blocked) = (self.sent, self.blocked);
        let acked_rate = self.acked_rate();
        self.sent = 0;
        self.blocked = 0;
        self.acks.clear();
        let next = if queueing > Self::QUEUE_HIGH_MS || starved {
            self.slow_start = false;
            let per_frame = acked_rate
                .map(|rate| 0.85 * rate / (self.streams as f64 * pace.max(1) as f64));
            let target = match per_frame {
                Some(bits) => bits as u32,
                None => self.current / 4 * 3,
            };
            target.clamp(self.current / 2, self.current / 100 * 90).max(self.min)
        } else if queueing < Self::QUEUE_LOW_MS {
            let (step, after) = if self.slow_start {
                (125, Duration::from_millis(500))
            } else {
                (105, Self::GROW_AFTER)
            };
            if now.duration_since(self.last_change) >= after {
                (self.current as u64 * step / 100).min(self.max as u64) as u32
            } else {
                self.current
            }
        } else {
            self.current
        };
        if next == self.current {
            return false;
        }
        tracing::info!(
            from_kbit = self.current / 1000,
            to_kbit = next / 1000,
            pace,
            queueing_ms = format!("{queueing:.1}"),
            sent,
            blocked,
            acked_mbit = acked_rate.map(|r| format!("{:.1}", r / 1e6)),
            "adapting frame budget"
        );
        self.current = next;
        self.last_change = now;
        true
    }
}

impl RttEstimator {
    fn new() -> Self {
        Self {
            sent: VecDeque::new(),
            smoothed_ms: 30.0,
            base_ms: f64::MAX,
            window_min_ms: f64::MAX,
        }
    }

    /// Queueing delay seen over the window, then start a new window.
    fn take_queueing_ms(&mut self) -> f64 {
        let min = std::mem::replace(&mut self.window_min_ms, f64::MAX);
        if min == f64::MAX || self.base_ms == f64::MAX {
            0.0
        } else {
            min - self.base_ms
        }
    }

    fn on_sent(&mut self, frame_id: u64, bytes: usize) {
        self.sent.push_back((frame_id, Instant::now(), bytes));
    }

    /// Record an ack; returns the bytes of the frame it covers. Acks arrive
    /// in order, so the oldest unacked frame is the one acked.
    fn record(&mut self, _frame_id: u64, _decoded_at_ms: u64) -> usize {
        let now = Instant::now();
        let Some((_, t, bytes)) = self.sent.pop_front() else {
            return 0;
        };
        let sample = now.duration_since(t).as_secs_f64() * 1000.0;
        self.smoothed_ms = 0.875 * self.smoothed_ms + 0.125 * sample.min(1000.0);
        self.window_min_ms = self.window_min_ms.min(sample);
        // The base follows the minimum, drifting up slowly so a path change
        // is re-learned.
        self.base_ms = (self.base_ms + 0.2).min(sample);
        bytes
    }

    /// How many frames may be unacked: enough to keep the path busy across
    /// its base RTT, but not more, so a queue cannot build behind a slow
    /// link. Queueing delay does not widen the window.
    fn window(&self, framerate: u32) -> u32 {
        let interval_ms = 1000.0 / framerate.max(1) as f64;
        let base = if self.base_ms == f64::MAX {
            self.smoothed_ms
        } else {
            self.base_ms
        };
        let n = (base / interval_ms).ceil() as i64 + 1;
        n.clamp(2, 8) as u32
    }
}

#[cfg(test)]
mod tests {
    use super::RttEstimator;

    #[test]
    fn queueing_does_not_widen_the_window() {
        let mut rtt = RttEstimator::new();
        rtt.base_ms = 20.0;
        rtt.smoothed_ms = 400.0;
        assert_eq!(rtt.window(60), 3);
    }

    #[test]
    fn ack_window_stays_in_bounds() {
        let rtt = RttEstimator::new();
        // With the default smoothed RTT the window is at least 2 and never
        // exceeds 8, for any frame rate.
        for fps in [1u32, 30, 60, 240] {
            let n = rtt.window(fps);
            assert!(
                (2..=8).contains(&n),
                "window {n} out of bounds at {fps} fps"
            );
        }
    }
}
