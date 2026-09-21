//! Environment probe: protocols, outputs, Vulkan, codec round-trip, capture,
//! and input injection. Every check prints PASS/FAIL lines.

use std::os::fd::AsFd;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
use wayland_client::globals::GlobalListContents;
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::{Connection, Dispatch, QueueHandle};

use gliff_proto::color::{bgra_to_yuv444, psnr, yuv444_to_bgra};
use gliff_vk::{Decoder, DmabufPlane, Encoder, EncoderSettings, Gpu, Reference};
use hypr_capture::{CaptureConfig, CaptureEvent, Capturer};
use hypr_input::{keys, Input, InputConfig, InputEvent};
use hypr_wl::Target;

#[derive(Parser)]
#[command(name = "gliff-probe", about = "Check that this machine can run gliff")]
struct Cli {
    /// Wayland socket name (defaults to WAYLAND_DISPLAY, then the Hyprland instance)
    #[arg(long, global = true)]
    display: Option<String>,
    /// Hyprland instance signature
    #[arg(long, global = true)]
    instance: Option<String>,
    /// DRM render node for GBM and Vulkan
    #[arg(long, global = true)]
    render_node: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List Wayland globals and check the ones gliff needs
    Protocols,
    /// List outputs (Wayland view and hyprctl view)
    Outputs,
    /// Report Hyprland permission settings that can block capture
    Permissions,
    /// Vulkan: device, queues and video capabilities
    Vulkan,
    /// Encode and decode synthetic BGRA frames end to end on the GPU
    Roundtrip {
        #[arg(long, default_value_t = 640)]
        width: u32,
        #[arg(long, default_value_t = 360)]
        height: u32,
        #[arg(long, default_value_t = 10)]
        frames: usize,
        /// Single 4:2:0 stream instead of Dual420 4:4:4.
        #[arg(long)]
        single: bool,
        /// Bits per second per stream (default: 4x the server default, as
        /// the synthetic stripes are a worst case for 4:2:0 chroma).
        #[arg(long)]
        bitrate: Option<u32>,
        /// Drop the bitrate to a quarter for the middle third of the run and
        /// restore it, to exercise the live rate-control update.
        #[arg(long)]
        adapt: bool,
        /// Withhold a few frames from the decoder and make the next one
        /// predict from the last frame it saw, to exercise recovery without
        /// a keyframe.
        #[arg(long)]
        lossy: bool,
    },
    /// Capture one frame of an output and write it as PNG
    Capture {
        #[arg(long)]
        output: Option<String>,
        #[arg(long, default_value = "capture.png")]
        png: PathBuf,
        /// Also wait for a cursor shape event
        #[arg(long)]
        cursor: bool,
    },
    /// Move the pointer to the middle of an output and type text
    Input {
        #[arg(long)]
        output: Option<String>,
        #[arg(long, default_value = "hello")]
        text: String,
        /// Click the left button at the centre before typing
        #[arg(long)]
        click: bool,
    },
    /// Capture one output frame and run it through the full Dual420 4:4:4
    /// pipeline: dmabuf import, GPU split, encode, decode, GPU recombine
    Pipeline {
        #[arg(long)]
        output: Option<String>,
    },
    /// Connect to a running `gliff-server --listen` and decode a few frames
    ServeTest {
        #[arg(long, default_value = "127.0.0.1:9000")]
        connect: String,
        #[arg(long, default_value_t = 30)]
        frames: usize,
    },
    /// Stream from a running `gliff-server --listen` for a while and report
    /// frame rate, interval jitter, end-to-end latency and bandwidth
    StreamBench {
        #[arg(long, default_value = "127.0.0.1:9000")]
        connect: String,
        #[arg(long, default_value_t = 10.0)]
        seconds: f64,
        /// Ask the server for this stream size (0 = leave it alone).
        #[arg(long, default_value_t = 0)]
        width: u32,
        #[arg(long, default_value_t = 0)]
        height: u32,
        /// Ack frames without decoding them (server-side throughput only).
        #[arg(long)]
        no_decode: bool,
        /// Write one CSV row per frame here.
        #[arg(long)]
        csv: Option<PathBuf>,
        /// Keep video on the connection instead of taking the UDP path.
        #[arg(long)]
        tcp: bool,
    },
    /// Watch or set the compositor's text clipboard (ext-data-control)
    Clipboard {
        /// Set the selection to this text and hold it, instead of watching.
        #[arg(long)]
        set: Option<String>,
        /// Seconds to run.
        #[arg(long, default_value_t = 3)]
        secs: u64,
    },
    /// Micro-benchmark the CPU colour/split reference the GPU shaders replace
    Bench {
        #[arg(long, default_value_t = 100)]
        iters: usize,
    },
    /// Run every non-interactive check
    All,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    let target = Target {
        display: cli.display.clone(),
        instance: cli.instance.clone(),
    };
    let node = hypr_capture::render_node(cli.render_node.as_deref());
    match cli.cmd {
        Cmd::Protocols => protocols(&target)?,
        Cmd::Outputs => outputs(&target)?,
        Cmd::Permissions => permissions(&target)?,
        Cmd::Vulkan => vulkan_info(&node)?,
        Cmd::Roundtrip {
            width,
            height,
            frames,
            single,
            bitrate,
            adapt,
            lossy,
        } => roundtrip(&node, width, height, frames, !single, bitrate, adapt, lossy)?,
        Cmd::Capture {
            output,
            png,
            cursor,
        } => capture(&target, &node, output, &png, cursor)?,
        Cmd::Input {
            output,
            text,
            click,
        } => input(&target, output, &text, click)?,
        Cmd::Pipeline { output } => pipeline(&target, &node, output)?,
        Cmd::ServeTest { connect, frames } => serve_test(&node, &connect, frames)?,
        Cmd::StreamBench {
            connect,
            seconds,
            width,
            height,
            no_decode,
            csv,
            tcp,
        } => stream_bench(
            &node,
            &connect,
            seconds,
            (width, height),
            no_decode,
            csv.as_deref(),
            tcp,
        )?,
        Cmd::Clipboard { set, secs } => clipboard(&target, set, secs)?,
        Cmd::Bench { iters } => bench(iters)?,
        Cmd::All => {
            protocols(&target)?;
            outputs(&target)?;
            permissions(&target)?;
            vulkan_info(&node)?;
            roundtrip(&node, 640, 360, 10, true, None, false, false)?;
            roundtrip(&node, 640, 360, 10, true, None, false, true)?;
        }
    }
    Ok(())
}

fn status(ok: bool, what: &str) {
    println!("{} {what}", if ok { "PASS" } else { "FAIL" });
}

fn protocols(target: &Target) -> Result<()> {
    struct S;
    impl Dispatch<WlRegistry, GlobalListContents> for S {
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
    let (_conn, globals, _queue) = hypr_wl::init::<S>(target)?;
    let list = hypr_wl::list_globals(&globals);
    for g in &list {
        println!("  {} v{}", g.interface, g.version);
    }
    let required: Vec<&str> = hypr_capture::REQUIRED_GLOBALS
        .iter()
        .chain(hypr_input::REQUIRED_GLOBALS)
        .copied()
        .collect();
    let mut all = true;
    for name in required {
        let ok = hypr_wl::has_global(&globals, name);
        all &= ok;
        status(ok, name);
    }
    let clipboard = hypr_wl::has_global(&globals, "ext_data_control_manager_v1")
        || hypr_wl::has_global(&globals, "zwlr_data_control_manager_v1");
    status(
        clipboard,
        "clipboard: ext_data_control_manager_v1 or zwlr_data_control_manager_v1",
    );
    status(
        hypr_wl::has_global(&globals, "zwp_keyboard_shortcuts_inhibit_manager_v1"),
        "zwp_keyboard_shortcuts_inhibit_manager_v1 (client side)",
    );
    if !all {
        bail!("required protocols missing");
    }
    Ok(())
}

fn outputs(target: &Target) -> Result<()> {
    for o in hypr_capture::list_outputs(target)? {
        let (lw, lh) = o.logical_size();
        println!(
            "  wl_output {} {}x{}@{}mHz scale {} logical {lw}x{lh} at {},{} ({})",
            o.name, o.width, o.height, o.refresh_mhz, o.scale, o.x, o.y, o.description
        );
    }
    match target.instance() {
        Ok(inst) => {
            for m in inst.monitors()? {
                println!(
                    "  hyprctl  {} {}x{}@{:.2} scale {} at {},{} focused={} disabled={}",
                    m.name,
                    m.width,
                    m.height,
                    m.refresh_rate,
                    m.scale,
                    m.x,
                    m.y,
                    m.focused,
                    m.disabled
                );
            }
        }
        Err(e) => println!("  hyprctl unavailable: {e}"),
    }
    Ok(())
}

fn permissions(target: &Target) -> Result<()> {
    let inst = target.instance()?;
    let enforce = inst.get_option("ecosystem:enforce_permissions")?;
    let on = enforce.as_bool().unwrap_or(false);
    status(!on, &format!("ecosystem:enforce_permissions = {on} (when on, add `permission = <gliff-server path>, screencopy, allow`)"));
    Ok(())
}

/// The colour bytes of a BGRA buffer, skipping the alpha/X byte whose captured
/// value is undefined.
fn rgb_channels(bgra: &[u8]) -> Vec<u8> {
    bgra.chunks_exact(4)
        .flat_map(|p| [p[0], p[1], p[2]])
        .collect()
}

fn pick_output(target: &Target, output: Option<String>) -> Result<String> {
    if let Some(o) = output {
        return Ok(o);
    }
    let list = hypr_capture::list_outputs(target)?;
    list.first()
        .map(|o| o.name.clone())
        .ok_or_else(|| anyhow!("no outputs"))
}

fn capture(
    target: &Target,
    node: &std::path::Path,
    output: Option<String>,
    png_path: &std::path::Path,
    cursor: bool,
) -> Result<()> {
    let output = pick_output(target, output)?;
    let mut cfg = CaptureConfig::new(output.clone());
    cfg.target = target.clone();
    cfg.render_node = node.to_path_buf();
    cfg.cursor = cursor;
    let (tx, rx) = mpsc::channel();
    let capturer = Capturer::start(
        cfg,
        Box::new(move |ev| {
            let _ = tx.send(ev);
        }),
    )?;
    capturer.request_frame()?;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut got_frame = false;
    let mut got_cursor = !cursor;
    while std::time::Instant::now() < deadline && !(got_frame && got_cursor) {
        let Ok(ev) = rx.recv_timeout(Duration::from_millis(200)) else {
            continue;
        };
        match ev {
            CaptureEvent::Ready {
                width,
                height,
                fourcc,
                modifier,
                ..
            } => {
                println!(
                    "  session ready {width}x{height} fourcc {:?} modifier {modifier:#x}",
                    fourcc.to_le_bytes().map(|b| b as char)
                );
            }
            CaptureEvent::Frame(frame) => {
                let image = frame.buffer.read_bgra()?;
                let (w, h) = (image.width as u32, image.height as u32);
                let rgb: Vec<u8> = image
                    .pixels
                    .chunks_exact(4)
                    .flat_map(|p| [p[2], p[1], p[0]])
                    .collect();
                write_png(png_path, w, h, &rgb)?;
                println!(
                    "  wrote {} ({w}x{h}, seq {}, damage {:?}, presentation {} ns)",
                    png_path.display(),
                    frame.sequence,
                    frame.damage,
                    frame.presentation_ns
                );
                got_frame = true;
            }
            CaptureEvent::CursorShape {
                width,
                height,
                hot_x,
                hot_y,
                argb,
            } => {
                let opaque = argb.chunks(4).filter(|p| p[3] > 0).count();
                println!(
                    "  cursor shape {width}x{height} hotspot {hot_x},{hot_y} ({opaque} opaque px)"
                );
                got_cursor = true;
            }
            CaptureEvent::CursorPos { x, y, visible } => {
                println!("  cursor pos {x},{y} visible={visible}")
            }
            CaptureEvent::Stopped => bail!("capture stopped"),
            CaptureEvent::Error(e) => bail!("capture error: {e}"),
        }
    }
    status(got_frame, "captured a frame");
    if cursor {
        status(got_cursor, "received a cursor shape");
    }
    if !got_frame {
        bail!("no frame within 5 s");
    }
    Ok(())
}

fn write_png(path: &std::path::Path, w: u32, h: u32, rgb: &[u8]) -> Result<()> {
    let file = std::fs::File::create(path)?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header()?;
    writer.write_image_data(rgb)?;
    Ok(())
}

fn input(target: &Target, output: Option<String>, text: &str, click: bool) -> Result<()> {
    let output = pick_output(target, output)?;
    let info = hypr_capture::list_outputs(target)?
        .into_iter()
        .find(|o| o.name == output)
        .ok_or_else(|| anyhow!("no output {output}"))?;
    let mut cfg = InputConfig::new(output.clone());
    cfg.target = target.clone();
    let (tx, rx) = mpsc::channel();
    let input = Input::start(
        cfg,
        Box::new(move |ev| {
            let _ = tx.send(ev);
        }),
    )?;
    if let Ok(InputEvent::Error(e)) = rx.recv_timeout(Duration::from_millis(100)) {
        bail!("input error: {e}");
    }
    let (lw, lh) = info.logical_size();
    input.motion(lw as f64 / 2.0, lh as f64 / 2.0)?;
    std::thread::sleep(Duration::from_millis(50));
    if click {
        input.button(keys::BTN_LEFT, true)?;
        std::thread::sleep(Duration::from_millis(30));
        input.button(keys::BTN_LEFT, false)?;
        std::thread::sleep(Duration::from_millis(100));
    }
    for ch in text.chars() {
        let code = match ch {
            'h' => keys::KEY_H,
            'e' => keys::KEY_E,
            'l' => keys::KEY_L,
            'o' => keys::KEY_O,
            ' ' => keys::KEY_SPACE,
            '\n' => keys::KEY_ENTER,
            other => bail!("no keycode for {other:?} in the smoke test"),
        };
        input.key(code, true)?;
        std::thread::sleep(Duration::from_millis(25));
        input.key(code, false)?;
        std::thread::sleep(Duration::from_millis(25));
    }
    input.release_all()?;
    std::thread::sleep(Duration::from_millis(50));
    status(
        true,
        &format!(
            "moved pointer to {},{} on {output} and typed {text:?}",
            lw / 2,
            lh / 2
        ),
    );
    Ok(())
}

fn serve_test(node: &std::path::Path, addr: &str, frames: usize) -> Result<()> {
    use gliff_proto::{ChromaMode, ClientCaps, ClientMsg, Codec, ServerMsg};
    use gliff_transport::Framed;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, async move {
        let stream = tokio::net::TcpStream::connect(addr).await.with_context(|| format!("connect {addr}"))?;
        stream.set_nodelay(true)?;
        let (rd, wr) = tokio::io::split(stream);
        let mut reader = Framed::new(rd);
        let mut writer = Framed::new(wr);
        let caps = ClientCaps { codecs: vec![Codec::H264], max_width: 1280, max_height: 720, chroma: vec![ChromaMode::Dual420, ChromaMode::Single420], udp: false };
        writer.write_msg(&ClientMsg::Hello { version: gliff_proto::PROTOCOL_VERSION, keymap: String::new(), caps }).await?;
        let ack = reader.read_msg::<ServerMsg>().await?;
        let ServerMsg::HelloAck { session, outputs, .. } = ack else { bail!("expected HelloAck, got {ack:?}") };
        eprintln!("  HelloAck: headless={} output={} ({} outputs)", session.headless, session.output, outputs.len());
        let cfg = reader.read_msg::<ServerMsg>().await?;
        let (mut w, mut h, chroma) = match cfg { ServerMsg::StreamConfig { width, height, chroma, .. } => (width as usize, height as usize, chroma), o => bail!("expected StreamConfig, got {o:?}") };
        eprintln!("  StreamConfig: {w}x{h} chroma {chroma:?}");
        let gpu = Gpu::open(Some(node))?;
        let mut decoder = Decoder::new(&gpu, chroma != ChromaMode::Single420, w as u32, h as u32)?;
        let mut got = 0usize;
        let mut keyframes = 0usize;
        let mut scaled = false;
        while got < frames {
            let msg = reader.read_msg::<ServerMsg>().await?;
            match msg {
                ServerMsg::VideoFrame { frame_id, keyframe, data_len, aux_len, .. } => {
                    let main = reader.read_payload(data_len).await?;
                    let aux = if aux_len > 0 { reader.read_payload(aux_len).await?.to_vec() } else { Vec::new() };
                    if keyframe { keyframes += 1; }
                    let out = decoder.decode_to_bgra(&main, &aux)?;
                    if out.is_some() { got += 1; }
                    if got == 1 { eprintln!("  first decoded frame ok ({}x{}, main {} aux {} bytes, key {keyframe})", w, h, data_len, aux_len); }
                    writer.write_msg(&ClientMsg::FrameAck { frame_id, decoded_at_ms: 0, held_ms: 0 }).await?;
                    if got == 3 { writer.write_msg(&ClientMsg::Resize { width: 800, height: 600, scale: 2.0 }).await?; }
                    if got == 4 {
                        if let Ok(text) = std::env::var("GLIFF_SEND_CLIP") {
                            let bytes = text.into_bytes();
                            writer.write_msg_with_payloads(&ClientMsg::ClipboardData { mime_type: "text/plain;charset=utf-8".into(), offset: 0, total: bytes.len() as u64, data_len: bytes.len() as u32 }, &[&bytes]).await?;
                        }
                    }
                }
                ServerMsg::StreamConfig { width, height, chroma, scale_milli, .. } => {
                    w = width as usize; h = height as usize;
                    eprintln!("  reconfig to {w}x{h} scale {scale_milli}");
                    scaled = if session.headless { scale_milli == 2000 } else { w <= 800 && h <= 600 && scale_milli < 1000 };
                    decoder = Decoder::new(&gpu, chroma != ChromaMode::Single420, w as u32, h as u32)?;
                }
                ServerMsg::CursorShape { argb_len, .. } => { let _ = reader.read_payload(argb_len).await?; }
                ServerMsg::ClipboardData { data_len, .. } => {
                    let bytes = reader.read_payload(data_len).await?;
                    if let Ok(t) = String::from_utf8(bytes.to_vec()) { eprintln!("CLIP-RECV: {t}"); }
                }
                ServerMsg::CursorPos { .. } | ServerMsg::Pong { .. } | ServerMsg::Error { .. } => {}
                _ => {}
            }
        }
        writer.write_msg(&ClientMsg::Bye).await?;
        eprintln!("RESULT decoded {got} frames, {keyframes} keyframes");
        status(got >= frames && keyframes >= 1, &format!("decoded {got} frames from the server ({keyframes} keyframes, resize honoured)"));
        if frames > 3 {
            let what = if session.headless { "server applied the requested output scale" } else { "server scaled the mirrored screen down to the window" };
            status(scaled, what);
        }
        Ok::<(), anyhow::Error>(())
    })?;
    Ok(())
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let i = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[i.min(sorted.len() - 1)]
}

fn stats(label: &str, unit: &str, mut v: Vec<f64>) {
    if v.is_empty() {
        println!("  {label:<18} n/a");
        return;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    let mean = v.iter().sum::<f64>() / v.len() as f64;
    println!(
        "  {label:<18} mean {mean:7.2} {unit}  p50 {:7.2}  p95 {:7.2}  max {:7.2}",
        percentile(&v, 0.5),
        percentile(&v, 0.95),
        v[v.len() - 1]
    );
}

#[allow(clippy::too_many_arguments)]
fn stream_bench(
    node: &std::path::Path,
    addr: &str,
    seconds: f64,
    size: (u32, u32),
    no_decode: bool,
    csv: Option<&std::path::Path>,
    tcp_only: bool,
) -> Result<()> {
    use gliff_proto::{ChromaMode, ClientCaps, ClientMsg, Codec, ServerMsg};
    use gliff_transport::recv::{spawn_server_reader, Incoming};
    use gliff_transport::udp::Packet;
    use gliff_transport::udp_io::{ack as udp_ack, spawn_client, ClientEvent};
    use gliff_transport::Framed;
    use std::collections::VecDeque;
    use std::io::Write;
    use std::time::Instant;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, async move {
        let stream = tokio::net::TcpStream::connect(addr).await.with_context(|| format!("connect {addr}"))?;
        stream.set_nodelay(true)?;
        let peer_ip = stream.peer_addr()?.ip();
        let (rd, wr) = tokio::io::split(stream);
        let mut reader = Framed::new(rd);
        let mut writer = Framed::new(wr);
        let caps = ClientCaps { codecs: vec![Codec::H264], max_width: 3840, max_height: 2160, chroma: vec![ChromaMode::Dual420, ChromaMode::Single420], udp: !tcp_only };
        writer.write_msg(&ClientMsg::Hello { version: gliff_proto::PROTOCOL_VERSION, keymap: String::new(), caps }).await?;
        let ack = reader.read_msg::<ServerMsg>().await?;
        let ServerMsg::HelloAck { session, udp: udp_offer, .. } = ack else { bail!("expected HelloAck, got {ack:?}") };
        let cfg = reader.read_msg::<ServerMsg>().await?;
        let (mut w, mut h, mut chroma) = match cfg { ServerMsg::StreamConfig { width, height, chroma, .. } => (width, height, chroma), o => bail!("expected StreamConfig, got {o:?}") };
        println!("  connected: headless={} output={} stream {w}x{h} {chroma:?}", session.headless, session.output);
        if size.0 > 0 && size.1 > 0 {
            writer.write_msg(&ClientMsg::Resize { width: size.0, height: size.1, scale: 1.0 }).await?;
        }
        let (mut udp_rx, udp_tx) = match udp_offer {
            Some(offer) if !tcp_only => {
                let (rx, tx) = spawn_client(vec![std::net::SocketAddr::new(peer_ip, offer.port)], offer.key, Duration::from_secs(6));
                (Some(rx), Some(tx))
            }
            _ => (None, None),
        };
        let gpu = Gpu::open(Some(node))?;
        let mut decoder = if no_decode { None } else { Some(Decoder::new(&gpu, chroma != ChromaMode::Single420, w, h)?) };
        let mut csv_out = match csv { Some(p) => Some(std::io::BufWriter::new(std::fs::File::create(p)?)), None => None };
        if let Some(c) = csv_out.as_mut() { writeln!(c, "t_ms,frame_id,key,bytes,latency_ms,decode_ms,path")?; }

        let start = Instant::now();
        let warmup = Duration::from_secs_f64(1.0);
        let deadline = start + Duration::from_secs_f64(seconds);
        let mut last_arrival: Option<Instant> = None;
        let mut intervals = Vec::new();
        let mut latency_recv = Vec::new();
        let mut latency_done = Vec::new();
        let mut decode_ms = Vec::new();
        let mut sizes = Vec::new();
        let mut frames = 0u64;
        let mut keyframes = 0u64;
        let mut recoveries = 0u64;
        let mut dropped = 0u64;
        let mut bytes = 0u64;
        let mut measured_from: Option<Instant> = None;
        let mut reconfigs = 0u32;
        let mut udp_active = false;
        let mut udp_stats = None;
        let mut decoded_ids: VecDeque<u64> = VecDeque::new();
        let mut tcp_rx = spawn_server_reader(reader);
        // One frame from either path: decode it if its reference is at
        // hand, ack it, and record the numbers.
        struct Frame<'a> { frame_id: u64, keyframe: bool, reference: Option<u64>, pts_us: u64, main: &'a [u8], aux: &'a [u8], held_ms: u32 }
        loop {
            let now = Instant::now();
            if now >= deadline { break; }
            let frame_from_tcp;
            let frame_from_udp;
            let f: Frame = tokio::select! {
                inc = tcp_rx.recv() => {
                    let Some(Incoming { msg, main, aux }) = inc else { break };
                    match msg {
                        ServerMsg::VideoFrame { frame_id, keyframe, reference, pts_us, .. } => {
                            frame_from_tcp = (main, aux);
                            Frame { frame_id, keyframe, reference, pts_us, main: &frame_from_tcp.0, aux: &frame_from_tcp.1, held_ms: 0 }
                        }
                        ServerMsg::StreamConfig { width, height, chroma: c, scale_milli, .. } => {
                            w = width; h = height; chroma = c; reconfigs += 1;
                            println!("  reconfig to {w}x{h} {chroma:?} scale {scale_milli}");
                            if decoder.is_some() { decoder = Some(Decoder::new(&gpu, chroma != ChromaMode::Single420, w, h)?); }
                            decoded_ids.clear();
                            last_arrival = None;
                            continue;
                        }
                        _ => continue,
                    }
                }
                ev = async { match udp_rx.as_mut() { Some(rx) => rx.recv().await, None => std::future::pending().await } } => {
                    match ev {
                        Some(ClientEvent::Up { rtt }) => {
                            println!("  UDP path up, rtt {:.1} ms", rtt.as_secs_f64() * 1000.0);
                            udp_active = true;
                            writer.write_msg(&ClientMsg::VideoPath { udp: true }).await?;
                            continue;
                        }
                        Some(ClientEvent::Frame(af)) => {
                            frame_from_udp = af;
                            Frame { frame_id: frame_from_udp.info.frame_id, keyframe: frame_from_udp.info.keyframe, reference: frame_from_udp.info.reference, pts_us: frame_from_udp.info.pts_us, main: &frame_from_udp.main, aux: &frame_from_udp.aux, held_ms: frame_from_udp.held.as_millis() as u32 }
                        }
                        Some(ClientEvent::Lost(_)) => {
                            recoveries += 1;
                            if let Some(tx) = &udp_tx { let _ = tx.send(Packet::Recover { last_good: decoded_ids.back().copied() }); }
                            continue;
                        }
                        Some(ClientEvent::Stats(s)) => { udp_stats = Some(s); continue; }
                        Some(ClientEvent::Down) | None => {
                            println!("  UDP path down; back on TCP");
                            if udp_active { writer.write_msg(&ClientMsg::VideoPath { udp: false }).await?; }
                            udp_active = false;
                            udp_rx = None;
                            continue;
                        }
                    }
                }
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => break,
            };
            let arrived = Instant::now();
            let now_us = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_micros() as i64;
            let lat_recv = (now_us - f.pts_us as i64) as f64 / 1000.0;
            let have_reference = f.keyframe || f.reference.is_some_and(|r| decoded_ids.contains(&r));
            let t0 = Instant::now();
            let mut dec = 0.0;
            let mut got_picture = true;
            if let Some(d) = decoder.as_mut() {
                if have_reference {
                    got_picture = d.decode(f.main, f.aux)?.is_some();
                    dec = t0.elapsed().as_secs_f64() * 1000.0;
                } else {
                    got_picture = false;
                }
            }
            match (&udp_tx, udp_active) {
                (Some(tx), true) => { let _ = tx.send(udp_ack(f.frame_id, f.held_ms)); }
                _ => writer.write_msg(&ClientMsg::FrameAck { frame_id: f.frame_id, decoded_at_ms: 0, held_ms: f.held_ms }).await?,
            }
            if !have_reference && decoder.is_some() {
                dropped += 1;
                recoveries += 1;
                let last_good = decoded_ids.back().copied();
                match (&udp_tx, udp_active) {
                    (Some(tx), true) => { let _ = tx.send(Packet::Recover { last_good }); }
                    _ => writer.write_msg(&ClientMsg::Recover { last_good }).await?,
                }
            }
            if got_picture {
                if f.keyframe { decoded_ids.clear(); }
                decoded_ids.push_back(f.frame_id);
                if decoded_ids.len() > gliff_vk::MAX_REFERENCES { decoded_ids.pop_front(); }
            } else { continue; }
            let in_window = arrived.duration_since(start) >= warmup;
            let total = f.main.len() + f.aux.len();
            if in_window {
                if measured_from.is_none() { measured_from = Some(arrived); }
                frames += 1;
                if f.keyframe { keyframes += 1; }
                bytes += total as u64;
                sizes.push(total as f64 / 1024.0);
                if let Some(prev) = last_arrival { intervals.push(arrived.duration_since(prev).as_secs_f64() * 1000.0); }
                latency_recv.push(lat_recv);
                latency_done.push(lat_recv + dec);
                if decoder.is_some() { decode_ms.push(dec); }
            }
            last_arrival = Some(arrived);
            if let Some(c) = csv_out.as_mut() {
                writeln!(c, "{:.1},{},{},{},{lat_recv:.2},{dec:.2},{}", arrived.duration_since(start).as_secs_f64() * 1000.0, f.frame_id, f.keyframe as u8, total, if udp_active { "udp" } else { "tcp" })?;
            }
        }
        writer.write_msg(&ClientMsg::Bye).await?;
        let span = measured_from.map(|t| last_arrival.unwrap_or(t).duration_since(t).as_secs_f64()).unwrap_or(0.0).max(0.001);
        let fps = frames as f64 / span;
        let mbit = bytes as f64 * 8.0 / 1e6 / span;
        let path = if udp_active { "udp" } else { "tcp" };
        println!("  {w}x{h} {chroma:?}: {frames} frames in {span:.1} s ({keyframes} keyframes, {reconfigs} reconfigs)");
        println!("  fps {fps:.1}   {mbit:.1} Mbit/s   decode={}   path {path}", !no_decode);
        let stalls = intervals.iter().filter(|&&ms| ms > 100.0).count();
        stats("interval", "ms", intervals);
        println!("  {:<18} {stalls}", "gaps over 100 ms");
        stats("latency to recv", "ms", latency_recv);
        stats("latency decoded", "ms", latency_done);
        stats("decode", "ms", decode_ms);
        stats("frame size", "KiB", sizes);
        println!("  {:<18} dropped {dropped} (no reference), recovery requests {recoveries}", "frames");
        if let Some(s) = udp_stats {
            println!("  {:<18} lost {} repaired {} nacks {}", "udp", s.lost, s.repaired, s.nacks);
        }
        println!("RESULT fps={fps:.1} mbit={mbit:.1} path={path} stalls={stalls}");
        Ok::<(), anyhow::Error>(())
    })?;
    Ok(())
}

fn clipboard(target: &Target, set: Option<String>, secs: u64) -> Result<()> {
    use hypr_input::{Clipboard, ClipboardEvent};
    use std::sync::mpsc;
    let (tx, rx) = mpsc::channel();
    let clip = Clipboard::start(
        target.clone(),
        Box::new(move |ev| {
            let _ = tx.send(ev);
        }),
    )?;
    if let Some(text) = set {
        clip.set_text(text.clone());
        println!("  set selection to {text:?}; holding {secs}s");
        std::thread::sleep(std::time::Duration::from_secs(secs));
        status(true, "clipboard set");
    } else {
        println!("  watching selection for {secs}s");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
        let mut got = false;
        while std::time::Instant::now() < deadline {
            if let Ok(ClipboardEvent::Text(t)) =
                rx.recv_timeout(std::time::Duration::from_millis(200))
            {
                println!("  selection: {t:?}");
                got = true;
            }
        }
        status(got, "observed a clipboard selection");
    }
    drop(clip);
    Ok(())
}

fn bench(iters: usize) -> Result<()> {
    use gliff_proto::chroma::{recombine_yuv444, split_yuv444, yuv444_to_nv12};
    use std::time::Instant;
    for (w, h) in [(1280usize, 720usize), (1920, 1080), (3840, 2160)] {
        // A representative BGRA frame.
        let mut bgra = vec![0u8; w * h * 4];
        for (i, px) in bgra.chunks_exact_mut(4).enumerate() {
            px[0] = (i & 0xff) as u8;
            px[1] = ((i >> 3) & 0xff) as u8;
            px[2] = ((i >> 6) & 0xff) as u8;
            px[3] = 255;
        }
        let time = |label: &str, n: usize, mut f: Box<dyn FnMut()>| {
            let t = Instant::now();
            for _ in 0..n {
                f();
            }
            let ms = t.elapsed().as_secs_f64() * 1000.0 / n as f64;
            println!(
                "  {w}x{h} {label:<28} {ms:6.2} ms/frame  ({:.0} fps cap)",
                1000.0 / ms.max(0.001)
            );
        };
        let src = bgra_to_yuv444(&bgra, w * 4, w, h);
        let (m, a) = split_yuv444(&src);
        time("bgra->yuv444 (server)", iters, {
            let bgra = bgra.clone();
            Box::new(move || {
                let _ = bgra_to_yuv444(&bgra, w * 4, w, h);
            })
        });
        time("split 4:4:4->2xNV12 (server)", iters, {
            let src = src.clone();
            Box::new(move || {
                let _ = split_yuv444(&src);
            })
        });
        time("subsample 4:4:4->NV12 (single)", iters, {
            let src = src.clone();
            Box::new(move || {
                let _ = yuv444_to_nv12(&src);
            })
        });
        time("recombine 2xNV12->444 (client)", iters, {
            let m = m.clone();
            let a = a.clone();
            Box::new(move || {
                let _ = recombine_yuv444(&m, &a);
            })
        });
        time("yuv444->bgra (client)", iters, {
            let src = src.clone();
            Box::new(move || {
                let _ = yuv444_to_bgra(&src);
            })
        });
    }
    Ok(())
}

fn vulkan_info(node: &std::path::Path) -> Result<()> {
    let gpu = Gpu::open(Some(node))?;
    println!("  {} ({})", gpu.name, gpu.driver);
    status(gpu.can_encode(), "Vulkan H.264 encode queue");
    if gpu.can_encode() {
        match Encoder::max_size(&gpu) {
            Ok((w, h)) => println!("  H.264 encode maximum {w}x{h}"),
            Err(e) => println!("  H.264 encode maximum unknown: {e}"),
        }
    }
    status(gpu.can_decode(), "Vulkan H.264 decode queue");
    Ok(())
}

/// A synthetic BGRA frame with sharp colour edges and motion, the case the
/// 4:4:4 path exists for.
fn synthetic_bgra(w: usize, h: usize, t: usize) -> Vec<u8> {
    let mut out = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let p = &mut out[(y * w + x) * 4..(y * w + x) * 4 + 4];
            let stripe = ((x + t * 3) / 32) % 4;
            let (b, g, r) = match stripe {
                0 => (255, 0, 0),
                1 => (0, 255, 0),
                2 => (0, 0, 255),
                _ => ((x * 255 / w) as u8, (y * 255 / h) as u8, 128),
            };
            p[0] = b;
            p[1] = g;
            p[2] = r;
            p[3] = 255;
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn roundtrip(
    node: &std::path::Path,
    width: u32,
    height: u32,
    frames: usize,
    dual: bool,
    bitrate: Option<u32>,
    adapt: bool,
    lossy: bool,
) -> Result<()> {
    let gpu = Gpu::open(Some(node))?;
    println!("  {} ({}) dual={dual}", gpu.name, gpu.driver);
    let bitrate = bitrate.unwrap_or(4 * EncoderSettings::default_bitrate(width, height, 60));
    let settings = EncoderSettings {
        width,
        height,
        bitrate,
        framerate: 60,
    };
    let mut encoder = Encoder::new(&gpu, settings, dual).context("vulkan encoder")?;
    let mut decoder = Decoder::new(&gpu, dual, width, height).context("vulkan decoder")?;
    let (w, h) = (width as usize, height as usize);
    // With --lossy the frames in `lost` never reach the decoder, and the
    // frame after them predicts from the last one before them.
    let lost = if lossy {
        let first = frames / 2 + 2;
        first..(first + 3).min(frames.saturating_sub(1))
    } else {
        0..0
    };
    let mut min_psnr = f64::MAX;
    let mut decoded = 0;
    let mut total_bytes = 0;
    let mut recovered = false;
    let start = std::time::Instant::now();
    for i in 0..frames {
        let src = synthetic_bgra(w, h, i);
        let force = i == frames / 2;
        let reference = if force {
            Reference::Keyframe
        } else if !lost.is_empty() && i == lost.end {
            Reference::Frame(lost.start as u64 - 1)
        } else {
            Reference::Latest
        };
        if adapt && i == frames / 3 {
            println!("  bitrate -> {}", bitrate / 4);
            encoder.set_bitrate(bitrate / 4);
        }
        if adapt && i == 2 * frames / 3 {
            println!("  bitrate -> {bitrate}");
            encoder.set_bitrate(bitrate);
        }
        let t0 = std::time::Instant::now();
        let packet = encoder
            .encode_bgra(&src, i as u64, reference)
            .with_context(|| format!("encode frame {i}"))?;
        let enc_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let aux = packet.aux.as_deref().unwrap_or(&[]);
        total_bytes += packet.main.len() + aux.len();
        if let Reference::Frame(from) = reference {
            recovered = packet.reference == Some(from) && !packet.keyframe;
            println!(
                "  frame {i}: predicts from frame {from} (got {:?}, key={})",
                packet.reference, packet.keyframe
            );
        }
        if lost.contains(&i) {
            println!("  frame {i}: {} bytes, withheld from the decoder", packet.main.len() + aux.len());
            continue;
        }
        let t1 = std::time::Instant::now();
        let out = decoder
            .decode_to_bgra(&packet.main, aux)
            .with_context(|| format!("decode frame {i}"))?;
        let dec_ms = t1.elapsed().as_secs_f64() * 1000.0;
        if i == 0 && !packet.keyframe {
            bail!("first packet is not a keyframe");
        }
        if force && !packet.keyframe {
            bail!("forced keyframe was not honoured");
        }
        let Some(out) = out else {
            println!("  frame {i}: no output");
            continue;
        };
        decoded += 1;
        // Compare against what the CPU reference path yields for the same
        // chroma mode, so 4:2:0's inherent loss is not counted against the GPU.
        let reference = {
            let yuv = bgra_to_yuv444(&src, w * 4, w, h);
            if dual {
                yuv444_to_bgra(&yuv)
            } else {
                yuv444_to_bgra(&gliff_proto::chroma::nv12_to_yuv444(
                    &gliff_proto::chroma::yuv444_to_nv12(&yuv),
                ))
            }
        };
        let p = psnr(&rgb_channels(&reference), &rgb_channels(&out));
        min_psnr = min_psnr.min(p);
        if let Some(dir) = std::env::var_os("GLIFF_VK_DUMP") {
            let dir = std::path::PathBuf::from(dir);
            let to_rgb = |b: &[u8]| -> Vec<u8> {
                b.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0]]).collect()
            };
            write_png(
                &dir.join(format!("src{i}.png")),
                width,
                height,
                &to_rgb(&src),
            )?;
            write_png(
                &dir.join(format!("out{i}.png")),
                width,
                height,
                &to_rgb(&out),
            )?;
        }
        println!("  frame {i}: main {} aux {} bytes key={} enc {enc_ms:.2} ms dec {dec_ms:.2} ms rgb psnr {p:.1} dB", packet.main.len(), aux.len(), packet.keyframe);
    }
    let elapsed = start.elapsed().as_secs_f64();
    println!(
        "  {frames} frames, {total_bytes} bytes, {:.1} fps end to end",
        frames as f64 / elapsed
    );
    let expected = frames - lost.len();
    status(
        decoded == expected,
        &format!("decoded {decoded}/{expected} frames"),
    );
    status(min_psnr > 30.0, &format!("min RGB PSNR {min_psnr:.1} dB"));
    if lossy {
        status(recovered, "recovery frame predicted from an older frame, no keyframe");
    }
    if decoded != expected || min_psnr <= 30.0 || (lossy && !recovered) {
        bail!("vulkan round-trip failed");
    }
    Ok(())
}

/// Capture one frame and push it through the exact server and client
/// pipelines: dmabuf import, GPU split, two encodes, two decodes, GPU
/// recombine. Compares the result with the CPU 4:4:4 reference.
fn pipeline(target: &Target, node: &std::path::Path, output: Option<String>) -> Result<()> {
    let output = pick_output(target, output)?;
    let mut cfg = CaptureConfig::new(output.clone());
    cfg.target = target.clone();
    cfg.render_node = node.to_path_buf();
    cfg.cursor = false;
    let (tx, rx) = mpsc::channel();
    let capturer = Capturer::start(
        cfg,
        Box::new(move |ev| {
            let _ = tx.send(ev);
        }),
    )?;
    capturer.request_frame()?;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut captured = None;
    while std::time::Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(CaptureEvent::Frame(frame)) => {
                captured = Some(frame);
                break;
            }
            Ok(CaptureEvent::Error(e)) => bail!("capture error: {e}"),
            Ok(_) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(_) => break,
        }
    }
    let frame = captured.ok_or_else(|| {
        anyhow!(
            "no frame captured (a headless output renders reliably; a physical KVM output may not)"
        )
    })?;
    let reference = frame.buffer.read_bgra()?;
    let (w, h) = (reference.width as u32, reference.height as u32);
    let info = &frame.buffer.info;
    let fourcc = drm_fourcc::DrmFourcc::try_from(info.fourcc)
        .map_err(|_| anyhow!("captured fourcc {:#x} is not a DRM format", info.fourcc))?;
    println!(
        "  captured {w}x{h} {fourcc:?} modifier {:#x} from {output}",
        info.modifier
    );
    let plane = DmabufPlane {
        fd: info.fd.as_fd(),
        width: info.width,
        height: info.height,
        offset: info.planes[0].offset,
        stride: info.planes[0].stride,
        fourcc,
        modifier: info.modifier,
    };

    let gpu = Gpu::open(Some(node))?;
    let encoder_max = Encoder::max_size(&gpu).context("encoder limits")?;
    let (sw, sh) = EncoderSettings::fit_extent(w, h, encoder_max);
    let scaled = (sw, sh) != (w, h);
    if scaled {
        println!(
            "  scaling {w}x{h} to {sw}x{sh} to fit the encoder maximum {}x{}",
            encoder_max.0, encoder_max.1
        );
    }
    let settings = EncoderSettings {
        width: sw,
        height: sh,
        bitrate: EncoderSettings::default_bitrate(sw, sh, 60),
        framerate: 60,
    };
    let mut encoder = Encoder::new(&gpu, settings, true).context("encoder")?;
    let mut decoder = Decoder::new(&gpu, true, sw, sh).context("decoder")?;
    let t0 = std::time::Instant::now();
    let packet = encoder
        .encode_dmabuf(1, &plane, 0, Reference::Keyframe)
        .context("encode")?;
    let enc_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let aux = packet.aux.as_deref().unwrap_or(&[]);
    println!(
        "  encoded main {} bytes, aux {} bytes, key={}, {enc_ms:.1} ms",
        packet.main.len(),
        aux.len(),
        packet.keyframe
    );
    let t1 = std::time::Instant::now();
    let out = decoder
        .decode_to_bgra(&packet.main, aux)
        .context("decode")?
        .ok_or_else(|| anyhow!("decode produced no frame"))?;
    let dec_ms = t1.elapsed().as_secs_f64() * 1000.0;
    drop(frame);
    drop(capturer);

    // What the CPU reference path makes of the same pixels, so only coding
    // loss and shader rounding count. A scaled stream is compared with a
    // CPU downscale, whose filter differs from the shader's, so the PSNR
    // is then informational and the check is that the round trip ran.
    let pixels = if scaled {
        downscale_bgra(
            &reference.pixels,
            reference.width,
            reference.height,
            sw as usize,
            sh as usize,
        )
    } else {
        reference.pixels.clone()
    };
    let cpu = yuv444_to_bgra(&bgra_to_yuv444(
        &pixels,
        sw as usize * 4,
        sw as usize,
        sh as usize,
    ));
    let rgb_psnr = psnr(&rgb_channels(&cpu), &rgb_channels(&out));
    println!("  decoded {dec_ms:.1} ms; end-to-end RGB PSNR vs CPU reference {rgb_psnr:.1} dB");
    let ok = scaled || rgb_psnr > 35.0;
    status(ok, "Dual420 4:4:4 GPU pipeline on a captured frame");
    if !ok {
        bail!("pipeline PSNR too low");
    }
    Ok(())
}

/// Area-average a BGRA image down to `dw`x`dh`.
fn downscale_bgra(src: &[u8], sw: usize, sh: usize, dw: usize, dh: usize) -> Vec<u8> {
    let mut out = vec![0u8; dw * dh * 4];
    for y in 0..dh {
        let y0 = y * sh / dh;
        let y1 = ((y + 1) * sh / dh).max(y0 + 1);
        for x in 0..dw {
            let x0 = x * sw / dw;
            let x1 = ((x + 1) * sw / dw).max(x0 + 1);
            let mut sum = [0u64; 4];
            for sy in y0..y1 {
                for sx in x0..x1 {
                    let p = &src[(sy * sw + sx) * 4..(sy * sw + sx) * 4 + 4];
                    for c in 0..4 {
                        sum[c] += p[c] as u64;
                    }
                }
            }
            let n = ((y1 - y0) * (x1 - x0)) as u64;
            let o = &mut out[(y * dw + x) * 4..(y * dw + x) * 4 + 4];
            for c in 0..4 {
                o[c] = ((sum[c] + n / 2) / n) as u8;
            }
        }
    }
    out
}
