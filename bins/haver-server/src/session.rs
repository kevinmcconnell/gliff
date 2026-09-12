//! One client session: output setup, the capture/encode/send loop, and input.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;

use haver_codec::color::bgra_to_yuv444;
use haver_codec::dual::DualEncoder;
use haver_codec::single::SingleEncoder;
use haver_codec::h264::EncoderSettings;
use haver_proto::{
    ChromaMode, ClientMsg, Codec, OutputInfo as ProtoOutput, Rect, ServerMsg, SessionInfo, PROTOCOL_VERSION,
};
use haver_transport::Framed;
use hypr_capture::{CaptureConfig, CaptureEvent, Capturer};
use hypr_input::{Axis as InAxis, Input, InputCmd, InputConfig};
use hypr_wl::Target;

pub struct Config {
    pub target: Target,
    pub output: Option<String>,
    pub render_node: PathBuf,
    pub low_bandwidth: bool,
    pub bitrate: Option<u32>,
}

/// A captured frame handed from the capture thread to the loop.
enum Incoming {
    Frame { bgra: Vec<u8>, width: usize, height: usize },
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

    // 1. Hello.
    let hello = reader.read_msg::<ClientMsg>().await.context("read Hello")?;
    let (keymap, caps) = match hello {
        ClientMsg::Hello { version, keymap, caps } => {
            if version != PROTOCOL_VERSION {
                writer.write_msg(&ServerMsg::Error { code: 1, message: format!("version {version} unsupported") }).await?;
                anyhow::bail!("client version {version} != {PROTOCOL_VERSION}");
            }
            (keymap, caps)
        }
        other => anyhow::bail!("expected Hello, got {other:?}"),
    };

    // 2. Output.
    let instance = cfg.target.instance().context("find Hyprland instance")?;
    let (output_name, mut width, mut height, created_headless) = setup_output(&instance, &cfg, &caps)?;
    tracing::info!(output = %output_name, width, height, headless = created_headless, "session output ready");

    // Remove a created headless output on every exit path, including any error
    // during the setup below (capture, input, encoder), not just a clean break.
    let _output_guard = OutputGuard {
        instance: if created_headless { Some((instance.clone(), output_name.clone())) } else { None },
    };

    // 3. Codec/chroma choice.
    let chroma = if cfg.low_bandwidth || !caps.chroma.contains(&ChromaMode::Dual420) {
        ChromaMode::Single420
    } else {
        ChromaMode::Dual420
    };
    let codec = Codec::H264;

    writer
        .write_msg(&ServerMsg::HelloAck {
            version: PROTOCOL_VERSION,
            session: SessionInfo { headless: created_headless, output: output_name.clone() },
            outputs: vec![ProtoOutput { name: output_name.clone(), width, height, scale_milli: 1000 }],
        })
        .await?;
    send_stream_config(&mut writer, codec, chroma, width, height).await?;

    // 4. Capture thread -> loop channel.
    let (cap_tx, mut cap_rx) = mpsc::unbounded_channel::<Incoming>();
    let render_node = cfg.render_node.clone();
    let capturer = start_capture(&cfg.target, &output_name, &render_node, cap_tx)?;

    // 5. Input thread.
    let input = start_input(&cfg.target, &output_name, &keymap)?;
    input.send(InputCmd::SetExtent { width, height }).ok();

    // 5b. Clipboard bridge (text). Compositor selection -> client, and back.
    let (clip_out_tx, mut clip_out_rx) = mpsc::unbounded_channel::<String>();
    let clipboard = match hypr_input::Clipboard::start(
        cfg.target.clone(),
        Box::new(move |ev| {
            let hypr_input::ClipboardEvent::Text(t) = ev;
            let _ = clip_out_tx.send(t);
        }),
    ) {
        Ok(c) => Some(c),
        Err(e) => {
            tracing::warn!(error = %e, "clipboard bridge unavailable");
            None
        }
    };

    // 6. Encoder.
    let display = haver_codec::vaapi::open_display(&render_node).context("open VA display")?;
    let mut settings = encoder_settings(width, height, cfg.bitrate);
    let mut encoder = Encoder::new(display.clone(), settings.clone(), chroma).context("create encoder")?;

    // 7. Loop state.
    let mut pending: Option<(Vec<u8>, usize, usize)> = None;
    let mut in_flight: u32 = 0;
    let mut n_limit: u32 = 2;
    let mut frame_id: u64 = 0;
    let mut want_keyframe = true;
    let mut rtt = RttEstimator::new();
    let mut cursor_shape_id: u32 = 0;
    capturer.request_frame().ok();
    let mut capture_asked = true;

    // Reads run on their own task so a burst of input (a mouse drag) can never
    // starve frame capture, and a blocked write can never block reads. The task
    // drains the socket into `msg_rx`; clipboard payloads are consumed inline.
    let (msg_tx, mut msg_rx) = mpsc::unbounded_channel::<ClientMsg>();
    let (clip_in_tx, mut clip_in_rx) = mpsc::unbounded_channel::<String>();
    tokio::task::spawn_local(async move {
        loop {
            match reader.read_msg::<ClientMsg>().await {
                Ok(ClientMsg::ClipboardData { data_len, .. }) => {
                    match reader.read_payload(data_len).await {
                        Ok(bytes) => {
                            if let Ok(text) = String::from_utf8(bytes.to_vec()) {
                                let _ = clip_in_tx.send(text);
                            }
                        }
                        Err(_) => break,
                    }
                }
                Ok(m) => {
                    if msg_tx.send(m).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    loop {
        tokio::select! {
            msg = msg_rx.recv() => {
                let Some(msg) = msg else { tracing::info!("client disconnected"); break; };
                match msg {
                    ClientMsg::Bye => break,
                    ClientMsg::FrameAck { frame_id: fid, decoded_at_ms } => {
                        in_flight = in_flight.saturating_sub(1);
                        rtt.record(fid, decoded_at_ms);
                        n_limit = rtt.window(settings.framerate);
                    }
                    ClientMsg::RequestKeyframe => want_keyframe = true,
                    ClientMsg::Key { keycode, pressed } => { input.send(InputCmd::Key { code: keycode, pressed }).ok(); }
                    ClientMsg::PointerMotion { x, y } => { input.send(InputCmd::Motion { x, y }).ok(); }
                    ClientMsg::PointerButton { button, pressed } => { input.send(InputCmd::Button { button, pressed }).ok(); }
                    ClientMsg::PointerAxis { axis, value, discrete, stop } => {
                        let axis = match axis { haver_proto::Axis::Vertical => InAxis::Vertical, haver_proto::Axis::Horizontal => InAxis::Horizontal };
                        input.send(InputCmd::Axis { axis, value, discrete, stop }).ok();
                    }
                    ClientMsg::Resize { width: rw, height: rh, .. } => {
                        let (rw, rh) = (rw & !1, rh & !1);
                        if created_headless && rw >= 320 && rh >= 240 && (rw != width || rh != height) {
                            tracing::info!(rw, rh, "resizing headless output");
                            instance.set_monitor_mode(&output_name, rw, rh, 60, 1.0).ok();
                            width = rw; height = rh;
                            settings = encoder_settings(width, height, cfg.bitrate);
                            encoder = Encoder::new(display.clone(), settings.clone(), chroma).context("reconfigure encoder")?;
                            input.send(InputCmd::SetExtent { width, height }).ok();
                            pending = None;
                            want_keyframe = true;
                            send_stream_config(&mut writer, codec, chroma, width, height).await?;
                        }
                    }
                    ClientMsg::Ping { t } => { writer.write_msg(&ServerMsg::Pong { t, server_now_ms: now_ms() }).await?; }
                    ClientMsg::ClipboardData { .. } | ClientMsg::ClipboardOffer { .. } | ClientMsg::ClipboardRequest { .. } => { /* clipboard: not yet wired */ }
                    ClientMsg::Hello { .. } => anyhow::bail!("unexpected second Hello"),
                }
            }
            text = clip_out_rx.recv() => {
                if let Some(text) = text {
                    let bytes = text.into_bytes();
                    let total = bytes.len() as u64;
                    writer.write_msg_with_payloads(
                        &ServerMsg::ClipboardData { mime_type: "text/plain;charset=utf-8".into(), offset: 0, total, data_len: bytes.len() as u32 },
                        &[&bytes],
                    ).await?;
                }
            }
            text = clip_in_rx.recv() => {
                if let (Some(text), Some(clip)) = (text, clipboard.as_ref()) {
                    clip.set_text(text);
                }
            }
            ev = cap_rx.recv() => {
                match ev {
                    Some(Incoming::Frame { bgra, width: fw, height: fh }) => {
                        pending = Some((bgra, fw, fh));
                        capture_asked = false;
                    }
                    Some(Incoming::Cursor { width, height, hot_x, hot_y, argb }) => {
                        cursor_shape_id += 1;
                        writer.write_msg_with_payloads(&ServerMsg::CursorShape { id: cursor_shape_id, width, height, hot_x, hot_y, argb_len: argb.len() as u32 }, &[&argb]).await?;
                    }
                    Some(Incoming::CursorPos { x, y, visible }) => {
                        writer.write_msg(&ServerMsg::CursorPos { x, y, shape_id: cursor_shape_id, visible }).await?;
                    }
                    Some(Incoming::Stopped) => { tracing::warn!("capture stopped"); break; }
                    Some(Incoming::Error(e)) => { tracing::error!(error=%e, "capture error"); break; }
                    None => break,
                }
            }
        }

        // Encode the latest captured frame if the client has ack capacity.
        if in_flight < n_limit {
            if let Some((bgra, fw, fh)) = pending.take() {
                if fw == width as usize && fh == height as usize {
                    let src = bgra_to_yuv444(&bgra, fw * 4, fw, fh);
                    let key = std::mem::take(&mut want_keyframe);
                    let t0 = Instant::now();
                    let (main, aux, keyframe) = encoder.encode(&src, frame_id, key)?;
                    let enc_us = t0.elapsed().as_micros();
                    let damage = vec![Rect { x: 0, y: 0, width: width as i32, height: height as i32 }];
                    let aux = aux.unwrap_or_default();
                    let aux_len = aux.len() as u32;
                    writer
                        .write_msg_with_payloads(
                            &ServerMsg::VideoFrame {
                                frame_id,
                                pts_us: now_ms() * 1000,
                                keyframe,
                                damage,
                                data_len: main.len() as u32,
                                aux_len,
                            },
                            &[&main, &aux],
                        )
                        .await?;
                    tracing::debug!(frame_id, key = keyframe, main = main.len(), aux = aux_len, enc_us, in_flight, n_limit, "sent frame");
                    rtt.on_sent(frame_id);
                    frame_id += 1;
                    in_flight += 1;
                }
            }
        }
        // Ask for the next frame when we have capacity and none is pending.
        if !capture_asked && pending.is_none() && in_flight < n_limit {
            capturer.request_frame().ok();
            capture_asked = true;
        }
    }

    input.send(InputCmd::ReleaseAll).ok();
    drop(input);
    drop(capturer);
    // `_output_guard` removes the headless output on drop.
    Ok(())
}

/// The active video encoder: full 4:4:4 over two streams, or a single 4:2:0
/// stream for `--low-bandwidth`.
#[allow(clippy::large_enum_variant)]
enum Encoder {
    Dual(DualEncoder),
    Single(SingleEncoder),
}

impl Encoder {
    fn new(display: std::rc::Rc<haver_codec::libva::Display>, settings: EncoderSettings, chroma: ChromaMode) -> Result<Self> {
        Ok(match chroma {
            ChromaMode::Single420 => Encoder::Single(SingleEncoder::new(display, settings)?),
            _ => Encoder::Dual(DualEncoder::new(display, settings)?),
        })
    }

    /// Encode one 4:4:4 frame; returns (main, optional aux, keyframe).
    fn encode(&mut self, src: &haver_proto::chroma::Yuv444, ts: u64, force: bool) -> Result<(Vec<u8>, Option<Vec<u8>>, bool)> {
        match self {
            Encoder::Dual(d) => {
                let p = d.encode(src, ts, force)?;
                Ok((p.main, Some(p.aux), p.keyframe))
            }
            Encoder::Single(s) => {
                let (main, key) = s.encode(src, ts, force)?;
                Ok((main, None, key))
            }
        }
    }
}

/// Removes a created headless output when the session ends, on any path.
struct OutputGuard {
    instance: Option<(hypr_ipc::Instance, String)>,
}

impl Drop for OutputGuard {
    fn drop(&mut self) {
        if let Some((instance, name)) = &self.instance {
            let _ = instance.remove_output(name);
        }
    }
}

async fn send_stream_config<W: AsyncWrite + Unpin>(writer: &mut Framed<W>, codec: Codec, chroma: ChromaMode, width: u32, height: u32) -> Result<()> {
    // Parameter sets ride in-band on every keyframe, so extradata is empty.
    writer
        .write_msg(&ServerMsg::StreamConfig { codec, chroma, width, height, extradata: Vec::new(), aux_extradata: None })
        .await?;
    Ok(())
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

/// Pick or create the output. Returns (name, width, height, created_headless).
fn setup_output(instance: &hypr_ipc::Instance, cfg: &Config, caps: &haver_proto::ClientCaps) -> Result<(String, u32, u32, bool)> {
    if let Some(name) = &cfg.output {
        let mons = instance.monitors()?;
        let m = mons.iter().find(|m| &m.name == name).with_context(|| format!("no output {name}"))?;
        return Ok((name.clone(), m.width & !1, m.height & !1, false));
    }
    // Headless: create one and find the new monitor name.
    let before: Vec<String> = instance.monitors()?.into_iter().map(|m| m.name).collect();
    let unique = format!("haver-{}", std::process::id());
    instance.create_headless_output(&unique).context("create headless output")?;
    std::thread::sleep(Duration::from_millis(200));
    let after = instance.monitors()?;
    let m = after
        .iter()
        .find(|m| !before.contains(&m.name))
        .context("headless output did not appear")?;
    let name = m.name.clone();
    let w = caps.max_width.clamp(320, 1920) & !1;
    let h = caps.max_height.clamp(240, 1080) & !1;
    instance.set_monitor_mode(&name, w, h, 60, 1.0).ok();
    std::thread::sleep(Duration::from_millis(150));
    Ok((name, w, h, true))
}

fn start_capture(target: &Target, output: &str, render_node: &std::path::Path, tx: mpsc::UnboundedSender<Incoming>) -> Result<Capturer> {
    let mut cc = CaptureConfig::new(output.to_string());
    cc.target = target.clone();
    cc.render_node = render_node.to_path_buf();
    cc.cursor = true;
    let sink = Box::new(move |ev: CaptureEvent| {
        let msg = match ev {
            CaptureEvent::Frame(frame) => {
                let info = &frame.buffer.info;
                let (w, h) = (info.width as usize & !1, info.height as usize & !1);
                let fourcc = info.fourcc;
                let bgr_order = matches!(&fourcc.to_le_bytes(), b"XR24" | b"AR24");
                let bgra = frame.buffer.with_mapped(|pixels, stride| {
                    let mut out = vec![0u8; w * h * 4];
                    for row in 0..h {
                        let line = &pixels[row * stride as usize..row * stride as usize + w * 4];
                        for col in 0..w {
                            let p = &line[col * 4..col * 4 + 4];
                            let d = &mut out[(row * w + col) * 4..(row * w + col) * 4 + 4];
                            if bgr_order {
                                d.copy_from_slice(p);
                            } else {
                                d[0] = p[2];
                                d[1] = p[1];
                                d[2] = p[0];
                                d[3] = p[3];
                            }
                        }
                    }
                    out
                });
                match bgra {
                    Ok(bgra) => Some(Incoming::Frame { bgra, width: w, height: h }),
                    Err(e) => Some(Incoming::Error(e.to_string())),
                }
            }
            CaptureEvent::CursorShape { width, height, hot_x, hot_y, argb } => Some(Incoming::Cursor { width, height, hot_x, hot_y, argb }),
            CaptureEvent::CursorPos { x, y, visible } => Some(Incoming::CursorPos { x: x as f64, y: y as f64, visible }),
            CaptureEvent::Stopped => Some(Incoming::Stopped),
            CaptureEvent::Error(e) => Some(Incoming::Error(e)),
            CaptureEvent::Ready { .. } => None,
        };
        if let Some(m) = msg {
            let _ = tx.send(m);
        }
    });
    Ok(Capturer::start(cc, sink)?)
}

fn start_input(target: &Target, output: &str, keymap: &str) -> Result<Input> {
    let mut ic = InputConfig::new(output.to_string());
    ic.target = target.clone();
    ic.keymap = if keymap.is_empty() { None } else { Some(keymap.to_string()) };
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
