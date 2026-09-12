//! Environment probe: protocols, outputs, VA-API, codec round-trip, capture,
//! and input injection. Every check prints PASS/FAIL lines.

use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
use wayland_client::globals::GlobalListContents;
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::{Connection, Dispatch, QueueHandle};

use haver_codec::frame::{FrameAllocator, FramePool};
use haver_codec::h264::{EncoderSettings, H264Decoder, H264Encoder};
use haver_codec::vaapi;
use hypr_capture::{CaptureConfig, CaptureEvent, Capturer};
use hypr_input::{keys, Input, InputConfig, InputEvent};
use hypr_wl::Target;

#[derive(Parser)]
#[command(name = "haver-probe", about = "Check that this machine can run haver")]
struct Cli {
    /// Wayland socket name (defaults to WAYLAND_DISPLAY, then the Hyprland instance)
    #[arg(long, global = true)]
    display: Option<String>,
    /// Hyprland instance signature
    #[arg(long, global = true)]
    instance: Option<String>,
    /// DRM render node for GBM and VA-API
    #[arg(long, global = true)]
    render_node: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List Wayland globals and check the ones haver needs
    Protocols,
    /// List outputs (Wayland view and hyprctl view)
    Outputs,
    /// Report Hyprland permission settings that can block capture
    Permissions,
    /// List VA-API profiles and entrypoints
    Vaapi,
    /// Encode and decode a synthetic NV12 stream and report PSNR
    Roundtrip {
        #[arg(long, default_value_t = 640)]
        width: u32,
        #[arg(long, default_value_t = 360)]
        height: u32,
        #[arg(long, default_value_t = 10)]
        frames: usize,
        #[arg(long, default_value_t = 20)]
        qp: u32,
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
    /// Run every non-interactive check
    All,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).with_writer(std::io::stderr).init();
    let cli = Cli::parse();
    let target = Target { display: cli.display.clone(), instance: cli.instance.clone() };
    let node = vaapi::render_node(cli.render_node.as_deref());
    match cli.cmd {
        Cmd::Protocols => protocols(&target)?,
        Cmd::Outputs => outputs(&target)?,
        Cmd::Permissions => permissions(&target)?,
        Cmd::Vaapi => vaapi_probe(&node)?,
        Cmd::Roundtrip { width, height, frames, qp } => roundtrip(&node, width, height, frames, qp)?,
        Cmd::Capture { output, png, cursor } => capture(&target, &node, output, &png, cursor)?,
        Cmd::Input { output, text, click } => input(&target, output, &text, click)?,
        Cmd::All => {
            protocols(&target)?;
            outputs(&target)?;
            permissions(&target)?;
            vaapi_probe(&node)?;
            roundtrip(&node, 640, 360, 10, 20)?;
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
        fn event(_: &mut Self, _: &WlRegistry, _: wayland_client::protocol::wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
    }
    let (_conn, globals, _queue) = hypr_wl::init::<S>(target)?;
    let list = hypr_wl::list_globals(&globals);
    for g in &list {
        println!("  {} v{}", g.interface, g.version);
    }
    let required: Vec<&str> = hypr_capture::REQUIRED_GLOBALS.iter().chain(hypr_input::REQUIRED_GLOBALS).copied().collect();
    let mut all = true;
    for name in required {
        let ok = hypr_wl::has_global(&globals, name);
        all &= ok;
        status(ok, name);
    }
    let clipboard = hypr_wl::has_global(&globals, "ext_data_control_manager_v1") || hypr_wl::has_global(&globals, "zwlr_data_control_manager_v1");
    status(clipboard, "clipboard: ext_data_control_manager_v1 or zwlr_data_control_manager_v1");
    status(hypr_wl::has_global(&globals, "zwp_keyboard_shortcuts_inhibit_manager_v1"), "zwp_keyboard_shortcuts_inhibit_manager_v1 (client side)");
    if !all {
        bail!("required protocols missing");
    }
    Ok(())
}

fn outputs(target: &Target) -> Result<()> {
    for o in hypr_capture::list_outputs(target)? {
        let (lw, lh) = o.logical_size();
        println!("  wl_output {} {}x{}@{}mHz scale {} logical {lw}x{lh} at {},{} ({})", o.name, o.width, o.height, o.refresh_mhz, o.scale, o.x, o.y, o.description);
    }
    match target.instance() {
        Ok(inst) => {
            for m in inst.monitors()? {
                println!("  hyprctl  {} {}x{}@{:.2} scale {} at {},{} focused={} disabled={}", m.name, m.width, m.height, m.refresh_rate, m.scale, m.x, m.y, m.focused, m.disabled);
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
    status(!on, &format!("ecosystem:enforce_permissions = {on} (when on, add `permission = <haver-server path>, screencopy, allow`)"));
    Ok(())
}

fn vaapi_probe(node: &std::path::Path) -> Result<()> {
    let info = vaapi::probe(node)?;
    println!("  {} : {}", info.node.display(), info.vendor);
    for p in &info.profiles {
        let eps: Vec<&str> = p.entrypoints.iter().map(|(_, n)| *n).collect();
        println!("  {:<24} {}", p.name, eps.join(","));
    }
    status(info.h264_encode(), "H.264 encode");
    status(info.h264_decode(), "H.264 decode");
    let native = info.native_444_encode();
    println!("INFO native 4:4:4 encode profiles: {}", if native.is_empty() { "none".to_owned() } else { native.join(",") });
    if !(info.h264_encode() && info.h264_decode()) {
        bail!("VA-API H.264 encode and decode are both required");
    }
    Ok(())
}

fn fill_synthetic(frame: &mut haver_codec::frame::Nv12Frame, w: u32, h: u32, t: usize) -> Result<()> {
    frame.with_planes_mut(|y, uv, py, puv| {
        for row in 0..h as usize {
            for col in 0..w as usize {
                y[row * py + col] = ((col + row + t * 4) & 0xff) as u8;
            }
        }
        for row in 0..(h / 2) as usize {
            for col in 0..(w / 2) as usize {
                uv[row * puv + col * 2] = ((col * 2 + t) & 0xff) as u8;
                uv[row * puv + col * 2 + 1] = ((row * 2) & 0xff) as u8;
            }
        }
    })?;
    Ok(())
}

fn psnr(a: &[u8], b: &[u8]) -> f64 {
    let mse: f64 = a.iter().zip(b).map(|(x, y)| (*x as f64 - *y as f64).powi(2)).sum::<f64>() / a.len() as f64;
    if mse == 0.0 {
        99.0
    } else {
        10.0 * (255.0f64 * 255.0 / mse).log10()
    }
}

fn roundtrip(node: &std::path::Path, width: u32, height: u32, frames: usize, qp: u32) -> Result<()> {
    let display = vaapi::open_display(node)?;
    let settings = EncoderSettings { width, height, qp, framerate: 60, low_power: false };
    let (cw, ch) = (settings.coded_width(), settings.coded_height());
    let alloc = FrameAllocator::open(node)?;
    let pool = FramePool::new(&alloc, cw, ch, 4)?;
    let mut encoder = H264Encoder::new(display.clone(), settings).context("create encoder")?;
    let dec_alloc = FrameAllocator::open(node)?;
    let mut decoder = H264Decoder::new(display, dec_alloc, 2).context("create decoder")?;
    let mut total_bytes = 0usize;
    let mut decoded = 0usize;
    let mut min_psnr = f64::MAX;
    let start = std::time::Instant::now();
    for i in 0..frames {
        let mut frame = pool.try_alloc()?;
        fill_synthetic(&mut frame, width, height, i)?;
        let mut source_y = vec![0u8; (width * height) as usize];
        frame.with_planes(|y, _, py, _| {
            for row in 0..height as usize {
                source_y[row * width as usize..(row + 1) * width as usize].copy_from_slice(&y[row * py..row * py + width as usize]);
            }
        })?;
        let force = i == frames / 2;
        let t0 = std::time::Instant::now();
        let packet = encoder.encode(frame, i as u64, force).with_context(|| format!("encode frame {i}"))?;
        let enc_ms = t0.elapsed().as_secs_f64() * 1000.0;
        total_bytes += packet.data.len();
        let t1 = std::time::Instant::now();
        let out = decoder.decode(i as u64, &packet.data).with_context(|| format!("decode frame {i}"))?;
        let dec_ms = t1.elapsed().as_secs_f64() * 1000.0;
        println!(
            "  frame {i}: {} bytes key={} enc {enc_ms:.2} ms dec {dec_ms:.2} ms -> {} frame(s) out",
            packet.data.len(),
            packet.keyframe,
            out.len()
        );
        if i == 0 && !packet.keyframe {
            bail!("first packet is not a keyframe");
        }
        if force && !packet.keyframe {
            bail!("forced keyframe was not honoured");
        }
        for d in out {
            decoded += 1;
            let mut y_out = vec![0u8; (width * height) as usize];
            d.frame.with_planes(|y, _, py, _| {
                for row in 0..height as usize {
                    y_out[row * width as usize..(row + 1) * width as usize].copy_from_slice(&y[row * py..row * py + width as usize]);
                }
            })?;
            let p = psnr(&source_y, &y_out);
            min_psnr = min_psnr.min(p);
            if d.timestamp != i as u64 {
                println!("  WARN decoded timestamp {} for input {i}: decoder is not zero-latency", d.timestamp);
            }
        }
    }
    let elapsed = start.elapsed().as_secs_f64();
    println!("  {frames} frames, {total_bytes} bytes, {:.1} fps end to end, extradata {} bytes", frames as f64 / elapsed, encoder.parameter_sets().len());
    status(decoded == frames, &format!("decoded {decoded}/{frames} frames with zero decoder latency"));
    status(min_psnr > 30.0, &format!("min luma PSNR {min_psnr:.1} dB"));
    if decoded != frames || min_psnr <= 30.0 {
        bail!("round-trip check failed");
    }
    Ok(())
}

fn pick_output(target: &Target, output: Option<String>) -> Result<String> {
    if let Some(o) = output {
        return Ok(o);
    }
    let list = hypr_capture::list_outputs(target)?;
    list.first().map(|o| o.name.clone()).ok_or_else(|| anyhow!("no outputs"))
}

fn capture(target: &Target, node: &std::path::Path, output: Option<String>, png_path: &std::path::Path, cursor: bool) -> Result<()> {
    let output = pick_output(target, output)?;
    let mut cfg = CaptureConfig::new(output.clone());
    cfg.target = target.clone();
    cfg.render_node = node.to_path_buf();
    cfg.cursor = cursor;
    let (tx, rx) = mpsc::channel();
    let capturer = Capturer::start(cfg, Box::new(move |ev| {
        let _ = tx.send(ev);
    }))?;
    capturer.request_frame()?;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut got_frame = false;
    let mut got_cursor = !cursor;
    while std::time::Instant::now() < deadline && !(got_frame && got_cursor) {
        let Ok(ev) = rx.recv_timeout(Duration::from_millis(200)) else { continue };
        match ev {
            CaptureEvent::Ready { width, height, fourcc, modifier, .. } => {
                println!("  session ready {width}x{height} fourcc {:?} modifier {modifier:#x}", fourcc.to_le_bytes().map(|b| b as char));
            }
            CaptureEvent::Frame(frame) => {
                let info = &frame.buffer.info;
                let (w, h) = (info.width, info.height);
                let fourcc = info.fourcc;
                let rgb = frame.buffer.with_mapped(|pixels, stride| to_rgb(pixels, stride as usize, w, h, fourcc))?;
                write_png(png_path, w, h, &rgb)?;
                println!("  wrote {} ({w}x{h}, seq {}, damage {:?}, presentation {} ns)", png_path.display(), frame.sequence, frame.damage, frame.presentation_ns);
                got_frame = true;
            }
            CaptureEvent::CursorShape { width, height, hot_x, hot_y, argb } => {
                let opaque = argb.chunks(4).filter(|p| p[3] > 0).count();
                println!("  cursor shape {width}x{height} hotspot {hot_x},{hot_y} ({opaque} opaque px)");
                got_cursor = true;
            }
            CaptureEvent::CursorPos { x, y, visible } => println!("  cursor pos {x},{y} visible={visible}"),
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

fn to_rgb(pixels: &[u8], stride: usize, w: u32, h: u32, fourcc: u32) -> Vec<u8> {
    let bgr = matches!(&fourcc.to_le_bytes(), b"XR24" | b"AR24");
    let mut out = Vec::with_capacity((w * h * 3) as usize);
    for row in 0..h as usize {
        let line = &pixels[row * stride..row * stride + w as usize * 4];
        for px in line.chunks_exact(4) {
            if bgr {
                out.extend_from_slice(&[px[2], px[1], px[0]]);
            } else {
                out.extend_from_slice(&[px[0], px[1], px[2]]);
            }
        }
    }
    out
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
    let input = Input::start(cfg, Box::new(move |ev| {
        let _ = tx.send(ev);
    }))?;
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
    status(true, &format!("moved pointer to {},{} on {output} and typed {text:?}", lw / 2, lh / 2));
    Ok(())
}
