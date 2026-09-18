//! One client session: output setup, the capture/encode/send loop, and input.

use std::collections::VecDeque;
use std::ops::ControlFlow;
use std::os::fd::AsFd;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use bytes::Bytes;
use gliff_proto::clipboard::CHUNK;
use gliff_proto::{
    ChromaMode, ClientCaps, ClientMsg, ClipboardMsg, Codec, OutputInfo as ProtoOutput, Rect,
    ServerMsg, SessionInfo, VideoPipeline, PROTOCOL_VERSION,
};
use gliff_sw::VideoMode;
use gliff_transport::clipboard::progress::Jobs;
use gliff_transport::clipboard::{outbound_channel, Transfers};
use gliff_transport::Framed;
use gliff_vk::{DmabufPlane, EncodedFrame, Encoder, EncoderSettings, Gpu};

use crate::writer::Writer;
use hypr_capture::{CaptureConfig, CaptureEvent, CapturedFrame, Capturer};
use hypr_input::{Axis as InAxis, Clipboard, ClipboardEvent, Input, InputCmd, InputConfig};
use hypr_wl::Target;

use crate::clipboard::Bridge;
use crate::notify;

pub struct Config {
    pub target: Target,
    pub output: Option<String>,
    pub render_node: PathBuf,
    pub low_bandwidth: bool,
    pub bitrate: Option<u32>,
    pub video: VideoMode,
    /// Dual-stream 4:4:4 on the CPU tier too (it defaults to Single420).
    pub full_chroma: bool,
}

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
    R: AsyncRead + Unpin + 'static,
    W: AsyncWrite + Unpin + 'static,
{
    let mut reader = Framed::new(rd);
    let mut writer = Framed::new(wr);

    let (keymap, caps) = handshake(&mut reader, &mut writer).await?;

    let instance = cfg.target.instance().context("find Hyprland instance")?;
    let output = setup_output(&instance, &cfg, &caps)?;
    tracing::info!(output = %output.name, output.width, output.height, headless = output.is_headless(), "session output ready");

    let video = VideoTier::open(cfg.video, &cfg.render_node);
    if !caps.chroma.contains(&ChromaMode::Dual420) && !caps.chroma.contains(&ChromaMode::Single420)
    {
        writer
            .write_msg(&ServerMsg::Error {
                code: 2,
                message: "no common chroma mode".into(),
            })
            .await?;
        anyhow::bail!("client advertises no chroma mode this server speaks");
    }
    // The CPU tier defaults to one 4:2:0 stream: dual-stream 4:4:4 doubles
    // the encode work, which the CPU pays for where the GPU does not. A
    // preference only applies when the client advertises the mode.
    let prefer_single = cfg.low_bandwidth || (matches!(video, VideoTier::Cpu) && !cfg.full_chroma);
    let chroma = if !caps.chroma.contains(&ChromaMode::Dual420)
        || (prefer_single && caps.chroma.contains(&ChromaMode::Single420))
    {
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
                scale_milli: (output.scale * 1000.0).round() as u32,
            }],
        })
        .await?;
    let encoder_max = video.encoder_max().context("query encoder limits")?;
    let stream = EncoderSettings::fit_extent(output.width, output.height, encoder_max);
    if stream != (output.width, output.height) {
        tracing::info!(
            width = stream.0,
            height = stream.1,
            "scaling the stream to the encoder maximum"
        );
    }
    writer
        .write_msg(&stream_config(
            codec,
            chroma,
            video.pipeline(),
            &output,
            stream,
        ))
        .await?;

    let (cap_tx, mut cap_rx) = mpsc::unbounded_channel();
    let capturer = start_capture(&cfg.target, &output.name, &cfg.render_node, cap_tx)?;

    let input = start_input(&cfg.target, &output.name, &keymap)?;
    input.send(output.logical_extent()).ok();
    if output.is_headless() {
        // Focus follows the pointer, so put it on the new screen right away:
        // otherwise the first launched window lands on the remote's own screen.
        let InputCmd::SetExtent { width, height } = output.logical_extent() else {
            unreachable!()
        };
        input
            .send(InputCmd::Motion {
                x: width as f64 / 2.0,
                y: height as f64 / 2.0,
            })
            .ok();
    }

    let (clip_ev_tx, mut clip_ev_rx) = mpsc::unbounded_channel::<ClipboardEvent>();
    let compositor_clipboard = Clipboard::start(
        cfg.target.clone(),
        Box::new(move |ev| {
            let _ = clip_ev_tx.send(ev);
        }),
    )
    .map_err(|e| tracing::warn!(error = %e, "clipboard bridge unavailable"))
    .ok();
    let (clip_out_tx, mut clip_out_rx) = outbound_channel();
    let (report_tx, report_rx) = mpsc::unbounded_channel();
    let jobs = Jobs::new(move |id, progress| {
        let _ = report_tx.send((id, progress));
    });
    notify::start(report_rx, jobs.clone());
    let clipboard = Bridge::new(Transfers::new(clip_out_tx), jobs, compositor_clipboard);

    let bitrate_ctl = match cfg.bitrate {
        Some(fixed) => BitrateController::new(fixed, true),
        None => BitrateController::new(
            EncoderSettings::default_bitrate(stream.0, stream.1, 60),
            false,
        ),
    };
    let settings = encoder_settings(stream.0, stream.1, bitrate_ctl.current());
    let encoder = VideoEncoder::new(&video, &settings, chroma == ChromaMode::Dual420)
        .context("create encoder")?;

    let mut msg_rx = spawn_reader(reader);
    let (writer, mut write_reports) = Writer::spawn(writer);

    capturer.request_frame().ok();
    let mut session = Session {
        writer,
        video,
        instance,
        output,
        stream,
        encoder_max,
        client_extent: (caps.max_width, caps.max_height),
        caps,
        bitrate_ctl,
        codec,
        chroma,
        settings,
        encoder,
        input,
        capturer,
        pending: None,
        capture_asked: true,
        blocked_noted: false,
        in_flight: 0,
        n_limit: 2,
        frame_id: 0,
        want_keyframe: true,
        rtt: RttEstimator::new(),
        cursor_shape_id: 0,
    };

    loop {
        let flow = tokio::select! {
            msg = msg_rx.recv() => match msg {
                Some(Inbound::Msg(msg)) => session.on_client_msg(msg).await?,
                Some(Inbound::Clipboard(msg, payload)) => {
                    clipboard.on_peer_msg(msg, payload);
                    ControlFlow::Continue(())
                }
                None => {
                    tracing::info!("client disconnected");
                    ControlFlow::Break(())
                }
            },
            out = clip_out_rx.recv() => {
                if let Some((msg, payload)) = out {
                    session.writer.send(ServerMsg::from(msg), vec![payload.to_vec()]);
                }
                ControlFlow::Continue(())
            }
            ev = clip_ev_rx.recv() => {
                if let Some(ev) = ev {
                    clipboard.on_compositor_event(ev);
                }
                ControlFlow::Continue(())
            }
            ev = cap_rx.recv() => session.on_capture(ev)?,
            report = write_reports.recv() => {
                report.context("writer task ended")??;
                ControlFlow::Continue(())
            }
        };
        if flow.is_break() {
            break;
        }
        session.pump_encoder()?;
    }

    session.inject(InputCmd::ReleaseAll);
    let Session {
        capturer,
        input,
        output,
        ..
    } = session;
    // Hyprland 0.56 aborts if a cursor capture session is still alive when
    // its monitor is removed, so end the capture and input threads before
    // dropping the output.
    drop(capturer);
    drop(input);
    drop(output);
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

/// What the reader task hands the session.
enum Inbound {
    Msg(ClientMsg),
    /// A clipboard message with the chunk that followed it (empty otherwise).
    Clipboard(ClipboardMsg, Bytes),
}

/// Drain the socket on its own task so a burst of input (a mouse drag) can
/// never starve frame capture and a blocked write can never block reads.
/// Clipboard chunks are read here so the socket stays framed.
fn spawn_reader<R>(mut reader: Framed<R>) -> UnboundedReceiver<Inbound>
where
    R: AsyncRead + Unpin + 'static,
{
    let (msg_tx, msg_rx) = mpsc::unbounded_channel();
    tokio::task::spawn_local(async move {
        loop {
            let msg = match reader.read_msg::<ClientMsg>().await {
                Ok(m) => m,
                Err(_) => break,
            };
            let inbound = match msg.into_clipboard() {
                Err(msg) => Inbound::Msg(msg),
                Ok(clip) => {
                    let payload = match &clip {
                        ClipboardMsg::Data { data_len, .. } if *data_len as usize <= CHUNK => {
                            match reader.read_payload(*data_len).await {
                                Ok(b) => b,
                                Err(_) => break,
                            }
                        }
                        ClipboardMsg::Data { .. } => break,
                        _ => Bytes::new(),
                    };
                    Inbound::Clipboard(clip, payload)
                }
            };
            if msg_tx.send(inbound).is_err() {
                break;
            }
        }
    });
    msg_rx
}

struct Session {
    writer: Writer,
    video: VideoTier,
    instance: hypr_ipc::Instance,
    output: SessionOutput,
    /// Encoded size. Equals the output size, except for a mirrored screen
    /// larger than the client's window, or any output larger than the
    /// encoder's maximum, which are scaled down to fit.
    stream: (u32, u32),
    /// The largest size the encoder accepts.
    encoder_max: (u32, u32),
    caps: ClientCaps,
    /// The client's last reported window size, so a mirrored output that
    /// changes mode can be refitted to the window.
    client_extent: (u32, u32),
    bitrate_ctl: BitrateController,
    codec: Codec,
    chroma: ChromaMode,
    settings: EncoderSettings,
    encoder: VideoEncoder,
    input: Input,
    capturer: Capturer,
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
}

impl Session {
    async fn on_client_msg(&mut self, msg: ClientMsg) -> Result<ControlFlow<()>> {
        match msg {
            ClientMsg::Bye => return Ok(ControlFlow::Break(())),
            ClientMsg::FrameAck {
                frame_id,
                decoded_at_ms,
            } => {
                self.in_flight = self.in_flight.saturating_sub(1);
                self.rtt.record(frame_id, decoded_at_ms);
                let n_limit = self.rtt.window(self.settings.framerate);
                if n_limit != self.n_limit {
                    tracing::debug!(
                        rtt_ms = format!("{:.1}", self.rtt.smoothed_ms),
                        n_limit,
                        "ack window changed"
                    );
                    self.n_limit = n_limit;
                }
                if let Some(bitrate) = self.bitrate_ctl.on_ack(self.rtt.smoothed_ms) {
                    self.settings.bitrate = bitrate;
                    self.encoder.set_bitrate(bitrate);
                }
            }
            ClientMsg::RequestKeyframe => self.want_keyframe = true,
            ClientMsg::Key { keycode, pressed } => self.inject(InputCmd::Key {
                code: keycode,
                pressed,
            }),
            ClientMsg::PointerMotion { x, y } => self.inject(InputCmd::Motion { x, y }),
            ClientMsg::PointerButton { button, pressed } => {
                self.inject(InputCmd::Button { button, pressed })
            }
            ClientMsg::PointerAxis {
                axis,
                value,
                discrete,
                stop,
            } => {
                let axis = match axis {
                    gliff_proto::Axis::Vertical => InAxis::Vertical,
                    gliff_proto::Axis::Horizontal => InAxis::Horizontal,
                };
                self.inject(InputCmd::Axis {
                    axis,
                    value,
                    discrete,
                    stop,
                })
            }
            ClientMsg::Resize {
                width,
                height,
                scale,
            } => self.resize(width, height, scale).await?,
            ClientMsg::Ping { t } => self.writer.send(
                ServerMsg::Pong {
                    t,
                    server_now_ms: now_ms(),
                },
                Vec::new(),
            ),
            // The reader task routes clipboard messages to the Bridge.
            ClientMsg::ClipboardData { .. }
            | ClientMsg::ClipboardOffer { .. }
            | ClientMsg::ClipboardRequest { .. }
            | ClientMsg::ClipboardAck { .. }
            | ClientMsg::ClipboardAbort { .. } => {}
            ClientMsg::Hello { .. } => anyhow::bail!("unexpected second Hello"),
        }
        Ok(ControlFlow::Continue(()))
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
        self.client_extent = (width, height);
        if !self.output.is_headless() {
            return self.fit_mirror(width, height);
        }
        let same_size = (width, height) == (self.output.width, self.output.height);
        let same_scale = (scale - self.output.scale).abs() < 0.01;
        if same_size && same_scale {
            return Ok(());
        }
        tracing::info!(width, height, scale, "resizing headless output");
        if let Err(e) = self
            .instance
            .set_monitor_mode(&self.output.name, width, height, 60, scale)
        {
            tracing::warn!(error = %e, "could not resize the headless output");
            return Ok(());
        }
        // Hyprland applies the mode asynchronously and may round the scale
        // so the logical size is whole; wait for it and use what it chose.
        let applied = self.wait_for_mode(width, height, scale).await;
        if !same_size {
            let stream = EncoderSettings::fit_extent(width, height, self.encoder_max);
            if stream != (width, height) {
                tracing::info!(
                    width = stream.0,
                    height = stream.1,
                    "scaling the stream to the encoder maximum"
                );
            }
            self.bitrate_ctl
                .retarget(EncoderSettings::default_bitrate(stream.0, stream.1, 60));
            let settings = encoder_settings(stream.0, stream.1, self.bitrate_ctl.current());
            match VideoEncoder::new(&self.video, &settings, self.chroma == ChromaMode::Dual420) {
                Ok(encoder) => {
                    self.encoder = encoder;
                    self.settings = settings;
                    self.output.width = width;
                    self.output.height = height;
                    self.stream = stream;
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
        self.writer.send(
            stream_config(
                self.codec,
                self.chroma,
                self.video.pipeline(),
                &self.output,
                self.stream,
            ),
            Vec::new(),
        );
        Ok(())
    }

    /// A mirrored screen keeps its size; when it is larger than the client's
    /// window the stream is scaled down to fit (never up), so the link and
    /// the decoder carry only what the window can show.
    fn fit_mirror(&mut self, win_w: u32, win_h: u32) -> Result<()> {
        let (ow, oh) = (self.output.width as f64, self.output.height as f64);
        let fit = (win_w as f64 / ow).min(win_h as f64 / oh).min(1.0);
        let stream = EncoderSettings::fit_extent(
            ((ow * fit).round() as u32).max(2) & !1,
            ((oh * fit).round() as u32).max(2) & !1,
            self.encoder_max,
        );
        if stream == self.stream {
            return Ok(());
        }
        self.bitrate_ctl
            .retarget(EncoderSettings::default_bitrate(stream.0, stream.1, 60));
        let settings = encoder_settings(stream.0, stream.1, self.bitrate_ctl.current());
        match VideoEncoder::new(&self.video, &settings, self.chroma == ChromaMode::Dual420) {
            Ok(encoder) => {
                tracing::info!(
                    width = stream.0,
                    height = stream.1,
                    "scaling the mirrored screen to the window"
                );
                self.encoder = encoder;
                self.settings = settings;
                self.stream = stream;
                self.pending = None;
                self.want_keyframe = true;
                self.writer.send(
                    stream_config(
                        self.codec,
                        self.chroma,
                        self.video.pipeline(),
                        &self.output,
                        self.stream,
                    ),
                    Vec::new(),
                );
                Ok(())
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

    fn on_capture(&mut self, ev: Option<Incoming>) -> Result<ControlFlow<()>> {
        match ev {
            Some(Incoming::Frame(image)) => {
                let size = (image.buffer.info.width & !1, image.buffer.info.height & !1);
                if !self.output.is_headless() && size != (self.output.width, self.output.height) {
                    self.output.width = size.0;
                    self.output.height = size.1;
                    if let Some(monitor) = self
                        .instance
                        .monitors()?
                        .iter()
                        .find(|m| m.name == self.output.name)
                    {
                        self.output.scale = monitor.scale;
                    }
                    self.inject(self.output.logical_extent());
                    let previous_stream = self.stream;
                    self.fit_mirror(self.client_extent.0, self.client_extent.1)?;
                    if self.stream == previous_stream {
                        self.writer.send(
                            stream_config(
                                self.codec,
                                self.chroma,
                                self.video.pipeline(),
                                &self.output,
                                self.stream,
                            ),
                            Vec::new(),
                        );
                        self.want_keyframe = true;
                    }
                    tracing::info!(width = size.0, height = size.1, "mirrored output resized");
                }
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
                self.writer.send(msg, vec![argb]);
            }
            Some(Incoming::CursorPos { x, y, visible }) => {
                self.writer.send(
                    ServerMsg::CursorPos {
                        x,
                        y,
                        shape_id: self.cursor_shape_id,
                        visible,
                    },
                    Vec::new(),
                );
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

    /// Encode and queue the pending frame if the client has ack capacity and
    /// the writer has no queued frame, then ask the capture thread for the
    /// next one.
    fn pump_encoder(&mut self) -> Result<()> {
        // Count a blocked frame once, not once per event-loop pass.
        if self.pending.is_some() && self.stalled() && !self.blocked_noted {
            self.bitrate_ctl.note_blocked();
            self.blocked_noted = true;
        }
        if !self.stalled() {
            if let Some(frame) = self.pending.take() {
                // A frame captured before a resize took effect is stale.
                let info = &frame.buffer.info;
                if (info.width & !1, info.height & !1) == (self.output.width, self.output.height) {
                    self.encode_and_send(&frame)?;
                }
            }
        }
        if !self.capture_asked && self.pending.is_none() && !self.stalled() {
            self.capturer.request_frame().ok();
            self.capture_asked = true;
        }
        Ok(())
    }

    /// The pacing gate: every allowed frame is unacked, or a frame write is
    /// still in the writer's queue.
    fn stalled(&self) -> bool {
        self.in_flight >= self.n_limit || !self.writer.video_ready()
    }

    fn encode_and_send(&mut self, frame: &CapturedFrame) -> Result<()> {
        let (width, height) = self.stream;
        let key = std::mem::take(&mut self.want_keyframe);
        let t0 = Instant::now();
        let encoded = self.encoder.encode(frame, self.stream, key)?;
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
        tracing::debug!(
            frame_id = self.frame_id,
            key = encoded.keyframe,
            main = encoded.main.len(),
            aux = aux.len(),
            enc_us,
            in_flight = self.in_flight,
            n_limit = self.n_limit,
            "queued frame"
        );
        self.writer.send(msg, vec![encoded.main, aux]);
        self.rtt.on_sent(self.frame_id);
        self.bitrate_ctl.note_sent();
        self.frame_id += 1;
        self.in_flight += 1;
        Ok(())
    }
}

/// The selected video pipeline: Vulkan Video on the GPU, or OpenH264 on the
/// CPU for machines without it.
enum VideoTier {
    Gpu(Arc<Gpu>),
    Cpu,
}

impl VideoTier {
    /// Open the requested tier. In `Gpu` mode a machine without a usable
    /// Vulkan Video encoder falls back to the CPU instead of failing.
    fn open(mode: VideoMode, render_node: &std::path::Path) -> Self {
        if mode == VideoMode::Cpu {
            tracing::info!("using the CPU video pipeline as requested");
            return Self::Cpu;
        }
        match Gpu::open(Some(render_node)) {
            Ok(gpu) if gpu.can_encode() => {
                tracing::info!(gpu = %gpu.name, "using the GPU video pipeline");
                Self::Gpu(gpu)
            }
            Ok(gpu) => {
                tracing::warn!(gpu = %gpu.name, "no Vulkan H.264 encode queue; falling back to the CPU pipeline");
                Self::Cpu
            }
            Err(e) => {
                tracing::warn!(error = %e, "no usable Vulkan device; falling back to the CPU pipeline");
                Self::Cpu
            }
        }
    }

    fn encoder_max(&self) -> Result<(u32, u32)> {
        match self {
            Self::Gpu(gpu) => Ok(Encoder::max_size(gpu)?),
            Self::Cpu => Ok(gliff_sw::Encoder::MAX_SIZE),
        }
    }

    fn pipeline(&self) -> VideoPipeline {
        match self {
            Self::Gpu(_) => VideoPipeline::Gpu,
            Self::Cpu => VideoPipeline::Cpu,
        }
    }
}

enum VideoEncoder {
    Gpu(Box<Encoder>),
    Cpu(Box<gliff_sw::Encoder>),
}

impl VideoEncoder {
    fn new(tier: &VideoTier, settings: &EncoderSettings, dual: bool) -> Result<Self> {
        match tier {
            VideoTier::Gpu(gpu) => Ok(Self::Gpu(Box::new(Encoder::new(
                gpu,
                settings.clone(),
                dual,
            )?))),
            VideoTier::Cpu => Ok(Self::Cpu(Box::new(gliff_sw::Encoder::new(
                gliff_sw::EncoderSettings {
                    width: settings.width,
                    height: settings.height,
                    bitrate: settings.bitrate,
                    framerate: settings.framerate,
                },
                dual,
            )?))),
        }
    }

    fn set_bitrate(&mut self, bitrate: u32) {
        match self {
            Self::Gpu(enc) => enc.set_bitrate(bitrate),
            Self::Cpu(enc) => enc.set_bitrate(bitrate),
        }
    }

    /// Encode a captured frame at `stream` size: the GPU imports the dmabuf
    /// and scales in its split shader; the CPU maps the buffer and scales
    /// the pixels before converting.
    fn encode(
        &mut self,
        frame: &CapturedFrame,
        stream: (u32, u32),
        force_keyframe: bool,
    ) -> Result<EncodedFrame> {
        match self {
            Self::Gpu(enc) => {
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
                // The import is cached per ring buffer; the generation changes
                // when the ring is reallocated (resize), so old imports are
                // never reused.
                let buffer_key = (frame.buffer.generation << 32) | frame.buffer.index as u64;
                Ok(enc.encode_dmabuf(buffer_key, &plane, force_keyframe)?)
            }
            Self::Cpu(enc) => {
                let image = frame.buffer.read_bgra()?;
                let (w, h) = (stream.0 as usize, stream.1 as usize);
                let pixels = if (image.width, image.height) == (w, h) {
                    image.pixels
                } else {
                    gliff_proto::color::downscale_bgra(
                        &image.pixels,
                        image.width,
                        image.height,
                        w,
                        h,
                    )
                };
                let f = enc.encode_bgra(&pixels, force_keyframe)?;
                Ok(EncodedFrame {
                    main: f.main,
                    aux: f.aux,
                    keyframe: f.keyframe,
                })
            }
        }
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
    pipeline: VideoPipeline,
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
        pipeline,
        width: stream.0,
        height: stream.1,
        scale_milli: (effective_scale * 1000.0).round() as u32,
        extradata: Vec::new(),
        aux_extradata: None,
    }
}

fn encoder_settings(width: u32, height: u32, bitrate: u32) -> EncoderSettings {
    EncoderSettings {
        width,
        height,
        bitrate,
        framerate: 60,
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
    let width = caps.max_width.clamp(320, 1920) & !1;
    let height = caps.max_height.clamp(240, 1080) & !1;
    if let Err(e) = instance.set_monitor_mode(&output.name, width, height, 60, 1.0) {
        tracing::warn!(error = %e, "could not set the headless output mode");
    }
    // Hyprland applies the mode asynchronously; stream whatever it settled
    // on rather than the size we asked for.
    let mut applied = None;
    for _ in 0..10 {
        std::thread::sleep(Duration::from_millis(50));
        let mons = instance.monitors()?;
        let Some(m) = mons.iter().find(|m| m.name == output.name) else {
            continue;
        };
        applied = Some((m.width, m.height, m.scale));
        if m.width == width && m.height == height {
            break;
        }
    }
    let (w, h, scale) = applied.context("headless output vanished")?;
    output.width = w & !1;
    output.height = h & !1;
    output.scale = scale;
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

/// Smoothed ack round-trip time, driving how many frames may be unacked.
struct RttEstimator {
    sent: VecDeque<(u64, Instant)>,
    smoothed_ms: f64,
}

/// Adapts the CBR target to the path. The ack RTT is the signal: its
/// minimum is the base delay, growth over the base is queueing; a queue or
/// a starved send window cuts the rate, a quiet path grows it back slowly.
struct BitrateController {
    min: u32,
    max: u32,
    current: u32,
    /// Set by `--bitrate`: the target does not follow the stream size.
    fixed: bool,
    base_rtt_ms: f64,
    last_eval: Instant,
    last_change: Instant,
    sent: u32,
    blocked: u32,
}

impl BitrateController {
    const EVAL_EVERY: Duration = Duration::from_millis(500);
    const GROW_AFTER: Duration = Duration::from_secs(2);
    const QUEUE_HIGH_MS: f64 = 50.0;
    const QUEUE_LOW_MS: f64 = 15.0;

    fn new(max: u32, fixed: bool) -> Self {
        let now = Instant::now();
        Self {
            min: (max / 8).max(1_000_000).min(max),
            max,
            current: max,
            fixed,
            base_rtt_ms: f64::MAX,
            last_eval: now,
            last_change: now,
            sent: 0,
            blocked: 0,
        }
    }

    fn current(&self) -> u32 {
        self.current
    }

    /// The stream size changed: keep the same share of the new ceiling, so
    /// bits per pixel stay constant across a resize.
    fn retarget(&mut self, max: u32) {
        if self.fixed || max == self.max {
            return;
        }
        let share = self.current as f64 / self.max as f64;
        self.max = max;
        self.min = (max / 8).max(1_000_000).min(max);
        self.current = ((max as f64 * share) as u32).clamp(self.min, self.max);
    }

    fn note_sent(&mut self) {
        self.sent += 1;
    }

    /// A frame is waiting because every allowed frame is still unacked.
    fn note_blocked(&mut self) {
        self.blocked += 1;
    }

    /// Feed the smoothed ack RTT; returns a new target when it changes.
    fn on_ack(&mut self, smoothed_ms: f64) -> Option<u32> {
        self.base_rtt_ms = self.base_rtt_ms.min(smoothed_ms);
        let now = Instant::now();
        if now.duration_since(self.last_eval) < Self::EVAL_EVERY {
            return None;
        }
        self.last_eval = now;
        // Let the base drift up slowly so a path change is re-learned.
        self.base_rtt_ms += 0.5;
        let queueing = smoothed_ms - self.base_rtt_ms;
        let starved = self.sent > 0 && self.blocked > self.sent;
        let (sent, blocked) = (self.sent, self.blocked);
        self.sent = 0;
        self.blocked = 0;
        let next = if queueing > Self::QUEUE_HIGH_MS || starved {
            (self.current / 4 * 3).max(self.min)
        } else if queueing < Self::QUEUE_LOW_MS
            && now.duration_since(self.last_change) >= Self::GROW_AFTER
        {
            (self.current / 10 * 11).min(self.max)
        } else {
            self.current
        };
        if next == self.current {
            return None;
        }
        tracing::info!(
            from = self.current,
            to = next,
            queueing_ms = format!("{queueing:.1}"),
            sent,
            blocked,
            "adapting bitrate"
        );
        self.current = next;
        self.last_change = now;
        Some(next)
    }
}

impl RttEstimator {
    fn new() -> Self {
        Self {
            sent: VecDeque::new(),
            smoothed_ms: 30.0,
        }
    }

    fn on_sent(&mut self, frame_id: u64) {
        self.sent.push_back((frame_id, Instant::now()));
    }

    fn record(&mut self, _frame_id: u64, _decoded_at_ms: u64) {
        // We approximate RTT from when we noticed the ack, not the client clock,
        // which avoids clock-skew: use the gap since the last ack as a proxy.
        let now = Instant::now();
        if let Some((_, t)) = self.sent.pop_front() {
            let sample = now.duration_since(t).as_secs_f64() * 1000.0;
            self.smoothed_ms = 0.875 * self.smoothed_ms + 0.125 * sample.min(1000.0);
        }
    }

    fn window(&self, framerate: u32) -> u32 {
        let interval_ms = 1000.0 / framerate.max(1) as f64;
        let n = (self.smoothed_ms / interval_ms).ceil() as i64 + 1;
        n.clamp(2, 8) as u32
    }
}

#[cfg(test)]
mod tests {
    use super::RttEstimator;

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
