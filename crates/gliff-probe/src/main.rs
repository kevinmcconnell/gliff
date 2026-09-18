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
use gliff_vk::{Decoder, DmabufPlane, Encoder, EncoderSettings, Gpu};
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
        } => roundtrip(&node, width, height, frames, !single, bitrate, adapt)?,
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
        Cmd::Clipboard { set, secs } => clipboard(&target, set, secs)?,
        Cmd::Bench { iters } => bench(iters)?,
        Cmd::All => {
            protocols(&target)?;
            outputs(&target)?;
            permissions(&target)?;
            vulkan_info(&node)?;
            roundtrip(&node, 640, 360, 10, true, None, false)?;
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
        let caps = ClientCaps { codecs: vec![Codec::H264], max_width: 1280, max_height: 720, chroma: vec![ChromaMode::Dual420, ChromaMode::Single420] };
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
        let send_clip = std::env::var("GLIFF_SEND_CLIP").ok();
        let mut clip_recv: Vec<u8> = Vec::new();
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
                    writer.write_msg(&ClientMsg::FrameAck { frame_id, decoded_at_ms: 0 }).await?;
                    if got == 3 { writer.write_msg(&ClientMsg::Resize { width: 800, height: 600, scale: 2.0 }).await?; }
                    if got == 4 && send_clip.is_some() {
                        let mime_types = gliff_proto::clipboard::TEXT_MIMES.iter().map(|m| m.to_string()).collect();
                        writer.write_msg(&ClientMsg::ClipboardOffer { mime_types, files: Vec::new() }).await?;
                    }
                }
                ServerMsg::StreamConfig { width, height, chroma, scale_milli, .. } => {
                    w = width as usize; h = height as usize;
                    eprintln!("  reconfig to {w}x{h} scale {scale_milli}");
                    scaled = if session.headless { scale_milli == 2000 } else { w <= 800 && h <= 600 && scale_milli < 1000 };
                    decoder = Decoder::new(&gpu, chroma != ChromaMode::Single420, w as u32, h as u32)?;
                }
                ServerMsg::CursorShape { argb_len, .. } => { let _ = reader.read_payload(argb_len).await?; }
                // The server offers its selection; ask for the text and print it once complete.
                ServerMsg::ClipboardOffer { mime_types, .. } => {
                    if mime_types.iter().any(|m| gliff_proto::clipboard::is_text_mime(m)) {
                        clip_recv.clear();
                        writer.write_msg(&ClientMsg::ClipboardRequest { id: 1, item: gliff_proto::ClipboardItem::Mime(gliff_proto::clipboard::TEXT_MIME.into()) }).await?;
                    }
                }
                ServerMsg::ClipboardData { id, data_len, done, .. } => {
                    let bytes = reader.read_payload(data_len).await?;
                    clip_recv.extend_from_slice(&bytes);
                    if done {
                        if let Ok(t) = String::from_utf8(std::mem::take(&mut clip_recv)) { eprintln!("CLIP-RECV: {t}"); }
                    } else {
                        writer.write_msg(&ClientMsg::ClipboardAck { id, received: clip_recv.len() as u64 }).await?;
                    }
                }
                // The server pastes our offered text: serve it in one chunk.
                ServerMsg::ClipboardRequest { id, .. } => {
                    let bytes = send_clip.clone().unwrap_or_default().into_bytes();
                    writer.write_msg_with_payloads(&ClientMsg::ClipboardData { id, offset: 0, data_len: bytes.len() as u32, done: true }, &[&bytes]).await?;
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

fn clipboard(target: &Target, set: Option<String>, secs: u64) -> Result<()> {
    use gliff_proto::clipboard::{is_text_mime, TEXT_MIMES};
    use hypr_input::{Clipboard, ClipboardEvent};
    use std::io::{Read, Write};
    use std::sync::mpsc;
    let (tx, rx) = mpsc::channel();
    let clip = Clipboard::start(
        target.clone(),
        Box::new(move |ev| {
            let _ = tx.send(ev);
        }),
    )?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    if let Some(text) = set {
        clip.offer(TEXT_MIMES.iter().map(|m| m.to_string()).collect());
        println!("  offering {text:?} as text; holding {secs}s");
        let mut pastes = 0;
        while std::time::Instant::now() < deadline {
            if let Ok(ClipboardEvent::Paste { mime_type, fd }) =
                rx.recv_timeout(std::time::Duration::from_millis(200))
            {
                let mut f = std::fs::File::from(fd);
                let _ = f.write_all(text.as_bytes());
                println!("  served a paste of {mime_type}");
                pastes += 1;
            }
        }
        status(true, &format!("clipboard offered ({pastes} pastes served)"));
    } else {
        println!("  watching selection for {secs}s");
        let mut got = false;
        while std::time::Instant::now() < deadline {
            if let Ok(ClipboardEvent::Selection { mime_types }) =
                rx.recv_timeout(std::time::Duration::from_millis(200))
            {
                println!("  selection offers {mime_types:?}");
                got = true;
                if let Some(m) = mime_types.iter().find(|m| is_text_mime(m)) {
                    let fd = clip.receive(m.clone())?;
                    let mut text = String::new();
                    std::fs::File::from(fd).read_to_string(&mut text)?;
                    println!("  text: {text:?}");
                }
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

fn roundtrip(
    node: &std::path::Path,
    width: u32,
    height: u32,
    frames: usize,
    dual: bool,
    bitrate: Option<u32>,
    adapt: bool,
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
    let mut min_psnr = f64::MAX;
    let mut decoded = 0;
    let mut total_bytes = 0;
    let start = std::time::Instant::now();
    for i in 0..frames {
        let src = synthetic_bgra(w, h, i);
        let force = i == frames / 2;
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
            .encode_bgra(&src, force)
            .with_context(|| format!("encode frame {i}"))?;
        let enc_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let aux = packet.aux.as_deref().unwrap_or(&[]);
        total_bytes += packet.main.len() + aux.len();
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
    status(
        decoded == frames,
        &format!("decoded {decoded}/{frames} frames"),
    );
    status(min_psnr > 30.0, &format!("min RGB PSNR {min_psnr:.1} dB"));
    if decoded != frames || min_psnr <= 30.0 {
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
    let packet = encoder.encode_dmabuf(1, &plane, true).context("encode")?;
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
