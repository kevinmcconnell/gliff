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

use gliff_proto::{
    ChromaMode, ClientCaps, ClientMsg, Codec, OutputInfo as ProtoOutput, Rect, ServerMsg,
    SessionInfo, PROTOCOL_VERSION,
};
use gliff_transport::Framed;
use gliff_vk::{DmabufPlane, Encoder, EncoderSettings, Gpu};
use hypr_capture::{CaptureConfig, CaptureEvent, CapturedFrame, Capturer};
use hypr_input::{Axis as InAxis, Clipboard, ClipboardEvent, Input, InputCmd, InputConfig};
use hypr_wl::Target;

use crate::output::Output;

pub struct Config {
    pub target: Target,
    pub output: Option<String>,
    pub render_node: PathBuf,
    pub low_bandwidth: bool,
    pub bitrate: Option<u32>,
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
    R: AsyncRead + Unpin + 'static,
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
                scale_milli: (output.scale * 1000.0).round() as u32,
            }],
        })
        .await?;
    let gpu = Gpu::open(Some(&cfg.render_node)).context("open Vulkan device")?;
    let encoder_max = Encoder::max_size(&gpu).context("query encoder limits")?;
    let stream = EncoderSettings::fit_extent(output.width, output.height, encoder_max);
    if stream != (output.width, output.height) {
        tracing::info!(
            width = stream.0,
            height = stream.1,
            "scaling the stream to the encoder maximum"
        );
    }
    writer
        .write_msg(&stream_config(codec, chroma, &output, stream))
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

    let (clip_out_tx, mut clip_out_rx) = mpsc::unbounded_channel::<String>();
    let clipboard = Clipboard::start(
        cfg.target.clone(),
        Box::new(move |ClipboardEvent::Text(t)| {
            let _ = clip_out_tx.send(t);
        }),
    )
    .map_err(|e| tracing::warn!(error = %e, "clipboard bridge unavailable"))
    .ok();

    let bitrate_ctl = match cfg.bitrate {
        Some(max) => BitrateController::new(max, true),
        None => BitrateController::new(
            EncoderSettings::default_bitrate(stream.0, stream.1, 60)
                .saturating_mul(stream_count(chroma)),
            false,
        ),
    };
    let settings = encoder_settings(
        stream.0,
        stream.1,
        bitrate_ctl.current() / stream_count(chroma),
    );
    let encoder = Encoder::new(&gpu, settings.clone(), chroma == ChromaMode::Dual420)
        .context("create encoder")?;

    let (mut msg_rx, mut clip_in_rx) = spawn_reader(reader);
    let (writer, mut writes) = Output::spawn(writer);
    let mut adaptation = tokio::time::interval(Duration::from_millis(250));
    adaptation.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    capturer.request_frame().ok();
    let mut session = Session {
        writer,
        gpu,
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
                Some(msg) => session.on_client_msg(msg).await?,
                None => {
                    tracing::info!("client disconnected");
                    ControlFlow::Break(())
                }
            },
            text = clip_out_rx.recv() => {
                if let Some(text) = text {
                    session.send_clipboard(text);
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
            result = writes.recv() => {
                session.bitrate_ctl.note_write(result.context("output writer stopped")??);
                ControlFlow::Continue(())
            },
            _ = adaptation.tick() => {
                if let Some(bitrate) = session.bitrate_ctl.evaluate(
                    Instant::now(), session.rtt.oldest_age(), session.writer.blocked_for(),
                ) {
                    let per_stream = bitrate / stream_count(session.chroma);
                    session.settings.bitrate = per_stream;
                    session.encoder.set_bitrate(per_stream);
                }
                ControlFlow::Continue(())
            },
        };
        if flow.is_break() {
            break;
        }
        session.pump_encoder()?
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

/// Drain the socket on its own task so a burst of input (a mouse drag) can
/// never starve frame capture and a blocked write can never block reads.
/// Clipboard payloads are consumed inline and delivered as text.
fn spawn_reader<R>(
    mut reader: Framed<R>,
) -> (UnboundedReceiver<ClientMsg>, UnboundedReceiver<String>)
where
    R: AsyncRead + Unpin + 'static,
{
    let (msg_tx, msg_rx) = mpsc::unbounded_channel();
    let (clip_tx, clip_rx) = mpsc::unbounded_channel();
    tokio::task::spawn_local(async move {
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
                    if msg_tx.send(msg).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
    (msg_rx, clip_rx)
}

struct Session {
    writer: Output,
    gpu: Arc<Gpu>,
    instance: hypr_ipc::Instance,
    output: SessionOutput,
    /// Encoded size. Equals the output size, except for a mirrored screen
    /// larger than the client's window, or any output larger than the
    /// encoder's maximum, which are scaled down to fit.
    stream: (u32, u32),
    /// The largest size the encoder accepts.
    encoder_max: (u32, u32),
    caps: ClientCaps,
    client_extent: (u32, u32),
    bitrate_ctl: BitrateController,
    codec: Codec,
    chroma: ChromaMode,
    settings: EncoderSettings,
    encoder: Encoder,
    input: Input,
    capturer: Capturer,
    /// The newest captured frame not yet encoded.
    pending: Option<CapturedFrame>,
    capture_asked: bool,
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
                decoded_at_ms: _,
            } => {
                let Some(sample) = self.rtt.record(frame_id) else {
                    return Ok(ControlFlow::Continue(()));
                };
                self.in_flight = self.in_flight.saturating_sub(1);
                self.bitrate_ctl.on_ack(sample);
                let n_limit = self.rtt.window(self.settings.framerate);
                if n_limit != self.n_limit {
                    tracing::debug!(
                        rtt_ms = format!("{:.1}", self.rtt.smoothed_ms),
                        n_limit,
                        "ack window changed"
                    );
                    self.n_limit = n_limit;
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
            ClientMsg::Ping { t } => {
                self.writer.send(
                    ServerMsg::Pong {
                        t,
                        server_now_ms: now_ms(),
                    },
                    Vec::new(),
                );
            }
            // ClipboardData is consumed by the reader task; the offer/request
            // negotiation is not used for text.
            ClientMsg::ClipboardData { .. }
            | ClientMsg::ClipboardOffer { .. }
            | ClientMsg::ClipboardRequest { .. } => {}
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
            self.bitrate_ctl.retarget(
                EncoderSettings::default_bitrate(stream.0, stream.1, 60)
                    .saturating_mul(stream_count(self.chroma)),
            );
            let settings = encoder_settings(
                stream.0,
                stream.1,
                self.bitrate_ctl.current() / stream_count(self.chroma),
            );
            match Encoder::new(
                &self.gpu,
                settings.clone(),
                self.chroma == ChromaMode::Dual420,
            ) {
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
            stream_config(self.codec, self.chroma, &self.output, self.stream),
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
        self.bitrate_ctl.retarget(
            EncoderSettings::default_bitrate(stream.0, stream.1, 60)
                .saturating_mul(stream_count(self.chroma)),
        );
        let settings = encoder_settings(
            stream.0,
            stream.1,
            self.bitrate_ctl.current() / stream_count(self.chroma),
        );
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
                self.stream = stream;
                self.pending = None;
                self.want_keyframe = true;
                self.writer.send(
                    stream_config(self.codec, self.chroma, &self.output, self.stream),
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

    fn send_clipboard(&mut self, text: String) {
        let bytes = text.into_bytes();
        let total = bytes.len() as u64;
        let msg = ServerMsg::ClipboardData {
            mime_type: TEXT_MIME.into(),
            offset: 0,
            total,
            data_len: bytes.len() as u32,
        };
        self.writer.send(msg, vec![bytes]);
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
                            stream_config(self.codec, self.chroma, &self.output, self.stream),
                            Vec::new(),
                        );
                        self.want_keyframe = true;
                    }
                    tracing::info!(width = size.0, height = size.1, "mirrored output resized");
                }
                self.pending = Some(image);
                self.capture_asked = false;
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

    /// Encode and send the pending frame if the client has ack capacity, then
    /// ask the capture thread for the next one.
    fn pump_encoder(&mut self) -> Result<()> {
        if self.in_flight < self.n_limit && self.writer.video_ready() {
            if let Some(frame) = self.pending.take() {
                // A frame captured before a resize took effect is stale.
                let info = &frame.buffer.info;
                if (info.width & !1, info.height & !1) == (self.output.width, self.output.height) {
                    self.encode_and_send(&frame)?;
                }
            }
        }
        if !self.capture_asked
            && self.pending.is_none()
            && self.in_flight < self.n_limit
            && self.writer.video_ready()
        {
            self.capturer.request_frame().ok();
            self.capture_asked = true;
        }
        Ok(())
    }

    fn encode_and_send(&mut self, frame: &CapturedFrame) -> Result<()> {
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
        let t0 = Instant::now();
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
        self.rtt.on_sent(self.frame_id);
        self.writer.send(msg, vec![encoded.main, aux]);
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

fn stream_count(chroma: ChromaMode) -> u32 {
    if chroma == ChromaMode::Dual420 {
        2
    } else {
        1
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

struct BitrateController {
    min: u32,
    max: u32,
    current: u32,
    fixed_ceiling: bool,
    base_rtt_ms: Option<f64>,
    smoothed_ms: f64,
    last_eval: Instant,
    last_change: Instant,
    acknowledged: bool,
    write_delay: Duration,
}

impl BitrateController {
    const EVAL_EVERY: Duration = Duration::from_millis(500);
    const GROW_AFTER: Duration = Duration::from_secs(2);
    const QUEUE_HIGH_MS: f64 = 50.0;
    const QUEUE_LOW_MS: f64 = 15.0;

    fn new(max: u32, fixed_ceiling: bool) -> Self {
        let now = Instant::now();
        Self {
            min: 250_000.min(max),
            max,
            current: 2_000_000.min(max),
            fixed_ceiling,
            base_rtt_ms: None,
            smoothed_ms: 0.0,
            last_eval: now,
            last_change: now,
            acknowledged: false,
            write_delay: Duration::ZERO,
        }
    }

    fn current(&self) -> u32 {
        self.current
    }

    fn retarget(&mut self, max: u32) {
        if self.fixed_ceiling || max == self.max {
            return;
        }
        self.max = max;
        self.min = 250_000.min(max);
        self.current = self.current.clamp(self.min, self.max);
    }

    fn note_write(&mut self, elapsed: Duration) {
        self.write_delay = self.write_delay.max(elapsed);
    }

    fn on_ack(&mut self, elapsed: Duration) {
        let sample = elapsed.as_secs_f64() * 1000.0;
        self.smoothed_ms = if self.base_rtt_ms.is_some() {
            0.875 * self.smoothed_ms + 0.125 * sample
        } else {
            sample
        };
        self.base_rtt_ms = Some(self.base_rtt_ms.map_or(sample, |base| base.min(sample)));
        self.acknowledged = true;
    }

    fn evaluate(&mut self, now: Instant, oldest: Duration, writing: Duration) -> Option<u32> {
        if now.duration_since(self.last_eval) < Self::EVAL_EVERY {
            return None;
        }
        self.last_eval = now;
        let base = self.base_rtt_ms.unwrap_or(250.0);
        let queueing = (self.smoothed_ms - base).max(0.0);
        let stalled = oldest.as_secs_f64() * 1000.0 > (base * 2.0).max(500.0);
        let write_ms = self.write_delay.max(writing).as_secs_f64() * 1000.0;
        let acknowledged = std::mem::take(&mut self.acknowledged);
        self.write_delay = Duration::ZERO;
        let next =
            if stalled || write_ms > 100.0 || (acknowledged && queueing > Self::QUEUE_HIGH_MS) {
                (self.current / 4 * 3).max(self.min)
            } else if acknowledged
                && queueing < Self::QUEUE_LOW_MS
                && now.duration_since(self.last_change) >= Self::GROW_AFTER
            {
                (self.current.saturating_add(self.current / 10)).min(self.max)
            } else {
                self.current
            };
        if acknowledged {
            if let Some(base) = &mut self.base_rtt_ms {
                *base += 0.5;
            }
        }
        if next == self.current {
            return None;
        }
        tracing::info!(
            from = self.current,
            to = next,
            queueing_ms = format!("{queueing:.1}"),
            write_ms = format!("{write_ms:.1}"),
            stalled,
            "adapting aggregate bitrate"
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

    fn record(&mut self, frame_id: u64) -> Option<Duration> {
        let position = self.sent.iter().position(|(id, _)| *id == frame_id)?;
        let (_, sent) = self.sent.remove(position)?;
        let elapsed = sent.elapsed();
        self.smoothed_ms = 0.875 * self.smoothed_ms + 0.125 * elapsed.as_secs_f64() * 1000.0;
        Some(elapsed)
    }

    fn oldest_age(&self) -> Duration {
        self.sent
            .front()
            .map_or(Duration::ZERO, |(_, sent)| sent.elapsed())
    }

    fn window(&self, framerate: u32) -> u32 {
        let interval_ms = 1000.0 / framerate.max(1) as f64;
        let n = (self.smoothed_ms / interval_ms).ceil() as i64 + 1;
        n.clamp(2, 8) as u32
    }
}

#[cfg(test)]
mod tests {
    use super::{BitrateController, RttEstimator};
    use std::time::Duration;

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
    #[test]
    fn stalls_reduce_bitrate_without_acknowledgements() {
        let mut ctl = BitrateController::new(24_000_000, false);
        assert_eq!(ctl.current(), 2_000_000);
        let start = ctl.last_eval;
        for tick in 1..20 {
            ctl.evaluate(
                start + Duration::from_millis(tick * 500),
                Duration::from_secs(tick),
                Duration::ZERO,
            );
        }
        assert_eq!(ctl.current(), 250_000);
    }

    #[test]
    fn blocked_writes_reduce_bitrate_even_with_timely_acks() {
        let mut ctl = BitrateController::new(24_000_000, false);
        ctl.on_ack(Duration::from_millis(250));
        ctl.note_write(Duration::from_millis(200));
        assert_eq!(
            ctl.evaluate(
                ctl.last_eval + Duration::from_millis(500),
                Duration::ZERO,
                Duration::ZERO
            ),
            Some(1_500_000)
        );
    }

    #[test]
    fn idle_sessions_do_not_probe_and_high_base_latency_is_not_congestion() {
        let mut ctl = BitrateController::new(24_000_000, false);
        let start = ctl.last_eval;
        assert_eq!(
            ctl.evaluate(
                start + Duration::from_secs(10),
                Duration::ZERO,
                Duration::ZERO
            ),
            None
        );
        ctl.on_ack(Duration::from_millis(300));
        assert_eq!(
            ctl.evaluate(
                start + Duration::from_secs(11),
                Duration::from_millis(300),
                Duration::ZERO
            ),
            Some(2_200_000)
        );
    }

    #[test]
    fn resize_preserves_learned_rate_and_explicit_ceiling() {
        let mut ctl = BitrateController::new(24_000_000, false);
        ctl.retarget(48_000_000);
        assert_eq!(ctl.current(), 2_000_000);
        ctl.retarget(100_000);
        assert_eq!(ctl.current(), 100_000);
        let mut explicit = BitrateController::new(500_000, true);
        explicit.retarget(48_000_000);
        assert_eq!(explicit.max, 500_000);
    }

    #[test]
    fn ack_matches_frame_and_includes_time_waiting_to_write() {
        let mut rtt = RttEstimator::new();
        rtt.on_sent(7);
        rtt.sent.front_mut().unwrap().1 -= Duration::from_millis(200);
        assert!(rtt.record(8).is_none());
        assert_eq!(rtt.sent.len(), 1);
        assert!(rtt.record(7).unwrap() >= Duration::from_millis(200));
        assert!(rtt.record(7).is_none());
        assert_eq!(rtt.oldest_age(), Duration::ZERO);
    }
}
