//! One client session: output setup, the capture/encode/send loop, and input.

use std::collections::VecDeque;
use std::ops::ControlFlow;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use haver_codec::color::bgra_to_yuv444;
use haver_codec::h264::EncoderSettings;
use haver_codec::libva::Display;
use haver_codec::Encoder;
use haver_proto::{
    ChromaMode, ClientCaps, ClientMsg, Codec, OutputInfo as ProtoOutput, Rect, ServerMsg, SessionInfo, PROTOCOL_VERSION,
};
use haver_transport::Framed;
use hypr_capture::{BgraImage, CaptureConfig, CaptureEvent, Capturer};
use hypr_input::{Axis as InAxis, Clipboard, ClipboardEvent, Input, InputCmd, InputConfig};
use hypr_wl::Target;

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
    Frame(BgraImage),
    Cursor { width: u32, height: u32, hot_x: i32, hot_y: i32, argb: Vec<u8> },
    CursorPos { x: f64, y: f64, visible: bool },
    Stopped,
    Error(String),
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64
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
            session: SessionInfo { headless: output.is_headless(), output: output.name.clone() },
            outputs: vec![ProtoOutput { name: output.name.clone(), width: output.width, height: output.height, scale_milli: 1000 }],
        })
        .await?;
    send_stream_config(&mut writer, codec, chroma, output.width, output.height).await?;

    let (cap_tx, mut cap_rx) = mpsc::unbounded_channel();
    let capturer = start_capture(&cfg.target, &output.name, &cfg.render_node, cap_tx)?;

    let input = start_input(&cfg.target, &output.name, &keymap)?;
    input.send(InputCmd::SetExtent { width: output.width, height: output.height }).ok();

    let (clip_out_tx, mut clip_out_rx) = mpsc::unbounded_channel::<String>();
    let clipboard = Clipboard::start(cfg.target.clone(), Box::new(move |ClipboardEvent::Text(t)| {
        let _ = clip_out_tx.send(t);
    }))
    .map_err(|e| tracing::warn!(error = %e, "clipboard bridge unavailable"))
    .ok();

    let display = haver_codec::vaapi::open_display(&cfg.render_node).context("open VA display")?;
    let settings = encoder_settings(output.width, output.height, cfg.bitrate);
    let encoder = Encoder::new(display.clone(), settings.clone(), chroma).context("create encoder")?;

    let (mut msg_rx, mut clip_in_rx) = spawn_reader(reader);

    capturer.request_frame().ok();
    let mut session = Session {
        writer,
        display,
        instance,
        output,
        caps,
        bitrate: cfg.bitrate,
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
                    session.send_clipboard(text).await?;
                }
                ControlFlow::Continue(())
            }
            text = clip_in_rx.recv() => {
                if let (Some(text), Some(clip)) = (text, &clipboard) {
                    clip.set_text(text);
                }
                ControlFlow::Continue(())
            }
            ev = cap_rx.recv() => session.on_capture(ev).await?,
        };
        if flow.is_break() {
            break;
        }
        session.pump_encoder().await?;
    }

    session.inject(InputCmd::ReleaseAll);
    // Dropping the session removes a created headless output.
    Ok(())
}

/// Read the client's Hello and check its protocol version.
async fn handshake<R, W>(reader: &mut Framed<R>, writer: &mut Framed<W>) -> Result<(String, ClientCaps)>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    match reader.read_msg::<ClientMsg>().await.context("read Hello")? {
        ClientMsg::Hello { version, keymap, caps } => {
            if version != PROTOCOL_VERSION {
                writer.write_msg(&ServerMsg::Error { code: 1, message: format!("version {version} unsupported") }).await?;
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
fn spawn_reader<R>(mut reader: Framed<R>) -> (UnboundedReceiver<ClientMsg>, UnboundedReceiver<String>)
where
    R: AsyncRead + Unpin + 'static,
{
    let (msg_tx, msg_rx) = mpsc::unbounded_channel();
    let (clip_tx, clip_rx) = mpsc::unbounded_channel();
    tokio::task::spawn_local(async move {
        loop {
            match reader.read_msg::<ClientMsg>().await {
                Ok(ClientMsg::ClipboardData { data_len, .. }) => match reader.read_payload(data_len).await {
                    Ok(bytes) => {
                        if let Ok(text) = String::from_utf8(bytes.to_vec()) {
                            let _ = clip_tx.send(text);
                        }
                    }
                    Err(_) => break,
                },
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

struct Session<W> {
    writer: Framed<W>,
    display: Rc<Display>,
    instance: hypr_ipc::Instance,
    output: SessionOutput,
    caps: ClientCaps,
    bitrate: Option<u32>,
    codec: Codec,
    chroma: ChromaMode,
    settings: EncoderSettings,
    encoder: Encoder,
    input: Input,
    capturer: Capturer,
    /// The newest captured frame not yet encoded.
    pending: Option<BgraImage>,
    capture_asked: bool,
    /// Frames sent but not yet acked; bounded by `n_limit` for pacing.
    in_flight: u32,
    n_limit: u32,
    frame_id: u64,
    want_keyframe: bool,
    rtt: RttEstimator,
    cursor_shape_id: u32,
}

impl<W: AsyncWrite + Unpin> Session<W> {
    async fn on_client_msg(&mut self, msg: ClientMsg) -> Result<ControlFlow<()>> {
        match msg {
            ClientMsg::Bye => return Ok(ControlFlow::Break(())),
            ClientMsg::FrameAck { frame_id, decoded_at_ms } => {
                self.in_flight = self.in_flight.saturating_sub(1);
                self.rtt.record(frame_id, decoded_at_ms);
                self.n_limit = self.rtt.window(self.settings.framerate);
            }
            ClientMsg::RequestKeyframe => self.want_keyframe = true,
            ClientMsg::Key { keycode, pressed } => self.inject(InputCmd::Key { code: keycode, pressed }),
            ClientMsg::PointerMotion { x, y } => self.inject(InputCmd::Motion { x, y }),
            ClientMsg::PointerButton { button, pressed } => self.inject(InputCmd::Button { button, pressed }),
            ClientMsg::PointerAxis { axis, value, discrete, stop } => {
                let axis = match axis {
                    haver_proto::Axis::Vertical => InAxis::Vertical,
                    haver_proto::Axis::Horizontal => InAxis::Horizontal,
                };
                self.inject(InputCmd::Axis { axis, value, discrete, stop })
            }
            ClientMsg::Resize { width, height, .. } => self.resize(width, height).await?,
            ClientMsg::Ping { t } => self.writer.write_msg(&ServerMsg::Pong { t, server_now_ms: now_ms() }).await?,
            // ClipboardData is consumed by the reader task; the offer/request
            // negotiation is not used for text.
            ClientMsg::ClipboardData { .. } | ClientMsg::ClipboardOffer { .. } | ClientMsg::ClipboardRequest { .. } => {}
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
    async fn resize(&mut self, width: u32, height: u32) -> Result<()> {
        let width = width.min(self.caps.max_width) & !1;
        let height = height.min(self.caps.max_height) & !1;
        if !self.output.is_headless() || width < 320 || height < 240 || (width, height) == (self.output.width, self.output.height) {
            return Ok(());
        }
        tracing::info!(width, height, "resizing headless output");
        self.instance.set_monitor_mode(&self.output.name, width, height, 60, 1.0).ok();
        self.output.width = width;
        self.output.height = height;
        self.settings = encoder_settings(width, height, self.bitrate);
        self.encoder = Encoder::new(self.display.clone(), self.settings.clone(), self.chroma).context("reconfigure encoder")?;
        self.inject(InputCmd::SetExtent { width, height });
        self.pending = None;
        self.want_keyframe = true;
        send_stream_config(&mut self.writer, self.codec, self.chroma, width, height).await
    }

    async fn send_clipboard(&mut self, text: String) -> Result<()> {
        let bytes = text.into_bytes();
        let total = bytes.len() as u64;
        let msg = ServerMsg::ClipboardData { mime_type: TEXT_MIME.into(), offset: 0, total, data_len: bytes.len() as u32 };
        Ok(self.writer.write_msg_with_payloads(&msg, &[&bytes]).await?)
    }

    async fn on_capture(&mut self, ev: Option<Incoming>) -> Result<ControlFlow<()>> {
        match ev {
            Some(Incoming::Frame(image)) => {
                self.pending = Some(image);
                self.capture_asked = false;
            }
            Some(Incoming::Cursor { width, height, hot_x, hot_y, argb }) => {
                self.cursor_shape_id += 1;
                let msg = ServerMsg::CursorShape { id: self.cursor_shape_id, width, height, hot_x, hot_y, argb_len: argb.len() as u32 };
                self.writer.write_msg_with_payloads(&msg, &[&argb]).await?;
            }
            Some(Incoming::CursorPos { x, y, visible }) => {
                self.writer.write_msg(&ServerMsg::CursorPos { x, y, shape_id: self.cursor_shape_id, visible }).await?;
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
    async fn pump_encoder(&mut self) -> Result<()> {
        if self.in_flight < self.n_limit {
            if let Some(image) = self.pending.take() {
                // A frame captured before a resize took effect is stale.
                if (image.width, image.height) == (self.output.width as usize, self.output.height as usize) {
                    self.encode_and_send(&image).await?;
                }
            }
        }
        if !self.capture_asked && self.pending.is_none() && self.in_flight < self.n_limit {
            self.capturer.request_frame().ok();
            self.capture_asked = true;
        }
        Ok(())
    }

    async fn encode_and_send(&mut self, image: &BgraImage) -> Result<()> {
        let (width, height) = (self.output.width, self.output.height);
        let src = bgra_to_yuv444(&image.pixels, image.width * 4, image.width, image.height);
        let key = std::mem::take(&mut self.want_keyframe);
        let t0 = Instant::now();
        let encoded = self.encoder.encode(&src, self.frame_id, key)?;
        let enc_us = t0.elapsed().as_micros();
        let aux = encoded.aux.unwrap_or_default();
        let msg = ServerMsg::VideoFrame {
            frame_id: self.frame_id,
            pts_us: now_ms() * 1000,
            keyframe: encoded.keyframe,
            damage: vec![Rect { x: 0, y: 0, width: width as i32, height: height as i32 }],
            data_len: encoded.main.len() as u32,
            aux_len: aux.len() as u32,
        };
        self.writer.write_msg_with_payloads(&msg, &[&encoded.main, &aux]).await?;
        tracing::debug!(
            frame_id = self.frame_id,
            key = encoded.keyframe,
            main = encoded.main.len(),
            aux = aux.len(),
            enc_us,
            in_flight = self.in_flight,
            n_limit = self.n_limit,
            "sent frame"
        );
        self.rtt.on_sent(self.frame_id);
        self.frame_id += 1;
        self.in_flight += 1;
        Ok(())
    }
}

/// The output being served. A created headless output is removed on drop, on
/// every exit path including a failure later in setup.
struct SessionOutput {
    name: String,
    width: u32,
    height: u32,
    headless: Option<hypr_ipc::Instance>,
}

impl SessionOutput {
    fn is_headless(&self) -> bool {
        self.headless.is_some()
    }
}

impl Drop for SessionOutput {
    fn drop(&mut self) {
        if let Some(instance) = &self.headless {
            let _ = instance.remove_output(&self.name);
        }
    }
}

async fn send_stream_config<W: AsyncWrite + Unpin>(writer: &mut Framed<W>, codec: Codec, chroma: ChromaMode, width: u32, height: u32) -> Result<()> {
    // Parameter sets ride in-band on every keyframe, so extradata is empty.
    let msg = ServerMsg::StreamConfig { codec, chroma, width, height, extradata: Vec::new(), aux_extradata: None };
    Ok(writer.write_msg(&msg).await?)
}

fn encoder_settings(width: u32, height: u32, bitrate: Option<u32>) -> EncoderSettings {
    EncoderSettings {
        width,
        height,
        bitrate: bitrate.unwrap_or_else(|| EncoderSettings::default_bitrate(width, height, 60)),
        framerate: 60,
        low_power: false,
    }
}

/// Pick the named output, or create a headless one sized to the client.
fn setup_output(instance: &hypr_ipc::Instance, cfg: &Config, caps: &ClientCaps) -> Result<SessionOutput> {
    if let Some(name) = &cfg.output {
        let mons = instance.monitors()?;
        let m = mons.iter().find(|m| &m.name == name).with_context(|| format!("no output {name}"))?;
        return Ok(SessionOutput { name: name.clone(), width: m.width & !1, height: m.height & !1, headless: None });
    }
    let before: Vec<String> = instance.monitors()?.into_iter().map(|m| m.name).collect();
    let requested = format!("haver-{}", std::process::id());
    instance.create_headless_output(&requested).context("create headless output")?;
    // Hyprland names the output itself, so find the one that appeared.
    let mut output = SessionOutput { name: requested, width: 0, height: 0, headless: Some(instance.clone()) };
    std::thread::sleep(Duration::from_millis(200));
    let after = instance.monitors()?;
    let m = after.iter().find(|m| !before.contains(&m.name)).context("headless output did not appear")?;
    output.name = m.name.clone();
    output.width = caps.max_width.clamp(320, 1920) & !1;
    output.height = caps.max_height.clamp(240, 1080) & !1;
    instance.set_monitor_mode(&output.name, output.width, output.height, 60, 1.0).ok();
    std::thread::sleep(Duration::from_millis(150));
    Ok(output)
}

fn start_capture(target: &Target, output: &str, render_node: &std::path::Path, tx: UnboundedSender<Incoming>) -> Result<Capturer> {
    let mut cc = CaptureConfig::new(output.to_string());
    cc.target = target.clone();
    cc.render_node = render_node.to_path_buf();
    cc.cursor = true;
    let sink = Box::new(move |ev: CaptureEvent| {
        let msg = match ev {
            CaptureEvent::Frame(frame) => match frame.buffer.read_bgra() {
                Ok(image) => Incoming::Frame(image),
                Err(e) => Incoming::Error(e.to_string()),
            },
            CaptureEvent::CursorShape { width, height, hot_x, hot_y, argb } => Incoming::Cursor { width, height, hot_x, hot_y, argb },
            CaptureEvent::CursorPos { x, y, visible } => Incoming::CursorPos { x: x as f64, y: y as f64, visible },
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

impl RttEstimator {
    fn new() -> Self {
        Self { sent: VecDeque::new(), smoothed_ms: 30.0 }
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
            assert!((2..=8).contains(&n), "window {n} out of bounds at {fps} fps");
        }
    }
}
