//! Environment probe: protocols, outputs, VA-API, codec round-trip, capture,
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

use haver_codec::color::{bgra_to_yuv444, psnr, yuv444_to_bgra};
use haver_codec::dual::{DualDecoder, DualEncoder};
use haver_codec::frame::{FrameAllocator, FramePool};
use haver_codec::gl_split::{nv12_planes, GlDualEncoder};
use haver_codec::h264::{EncoderSettings, H264Decoder, H264Encoder};
use haver_codec::{vaapi, Decoder};
use hypr_capture::{BgraImage, CaptureConfig, CaptureEvent, Capturer};
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
        /// Target bitrate in bits per second (default: derived from size)
        #[arg(long)]
        bitrate: Option<u32>,
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
    /// Capture one output frame and run it through the full Dual420 4:4:4 codec
    Pipeline {
        #[arg(long)]
        output: Option<String>,
        /// Do the 4:4:4 split on the GPU (GL into VA surfaces) and compare
        /// against the CPU split for correctness and speed.
        #[arg(long)]
        gl: bool,
    },
    /// Connect to a running `haver-server --listen` and decode a few frames
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
    /// Micro-benchmark the CPU colour/split stages the GPU path would replace
    Bench {
        #[arg(long, default_value_t = 100)]
        iters: usize,
    },
    /// Feasibility test: can GL render into a VA encoder surface (dmabuf)?
    GlTest,
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
    let node = vaapi::render_node(cli.render_node.as_deref());
    match cli.cmd {
        Cmd::Protocols => protocols(&target)?,
        Cmd::Outputs => outputs(&target)?,
        Cmd::Permissions => permissions(&target)?,
        Cmd::Vaapi => vaapi_probe(&node)?,
        Cmd::Roundtrip {
            width,
            height,
            frames,
            bitrate,
        } => roundtrip(&node, width, height, frames, bitrate)?,
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
        Cmd::Pipeline { output, gl } => pipeline(&target, &node, output, gl)?,
        Cmd::ServeTest { connect, frames } => serve_test(&node, &connect, frames)?,
        Cmd::Clipboard { set, secs } => clipboard(&target, set, secs)?,
        Cmd::Bench { iters } => bench(iters)?,
        Cmd::GlTest => gltest(&node)?,
        Cmd::All => {
            protocols(&target)?;
            outputs(&target)?;
            permissions(&target)?;
            vaapi_probe(&node)?;
            roundtrip(&node, 640, 360, 10, None)?;
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
    println!(
        "INFO native 4:4:4 encode profiles: {}",
        if native.is_empty() {
            "none".to_owned()
        } else {
            native.join(",")
        }
    );
    if !(info.h264_encode() && info.h264_decode()) {
        bail!("VA-API H.264 encode and decode are both required");
    }
    Ok(())
}

fn fill_synthetic(
    frame: &mut haver_codec::frame::Nv12Frame,
    w: u32,
    h: u32,
    t: usize,
) -> Result<()> {
    let shift = t as f64 * 3.0;
    frame.with_planes_mut(|y, uv, py, puv| {
        for row in 0..h as usize {
            for col in 0..w as usize {
                let base = 16.0 + 200.0 * (col as f64 / w as f64);
                let wave = 16.0 * ((row as f64 / 24.0 + shift).sin());
                y[row * py + col] = (base + wave).clamp(16.0, 235.0) as u8;
            }
        }
        for row in 0..(h / 2) as usize {
            for col in 0..(w / 2) as usize {
                uv[row * puv + col * 2] = 128;
                uv[row * puv + col * 2 + 1] = 128;
            }
        }
    })?;
    Ok(())
}

/// The colour bytes of a BGRA buffer, skipping the alpha/X byte whose captured
/// value is undefined.
fn rgb_channels(bgra: &[u8]) -> Vec<u8> {
    bgra.chunks_exact(4)
        .flat_map(|p| [p[0], p[1], p[2]])
        .collect()
}

fn roundtrip(
    node: &std::path::Path,
    width: u32,
    height: u32,
    frames: usize,
    bitrate: Option<u32>,
) -> Result<()> {
    let display = vaapi::open_display(node)?;
    let bitrate = bitrate.unwrap_or_else(|| EncoderSettings::default_bitrate(width, height, 60));
    let settings = EncoderSettings {
        width,
        height,
        bitrate,
        framerate: 60,
        low_power: false,
    };
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
        let source_y = frame.read_nv12(width as usize, height as usize)?.y;
        let force = i == frames / 2;
        let (yb, uvb, yp, uvp) =
            frame.with_planes(|y, uv, py, puv| (y.to_vec(), uv.to_vec(), py, puv))?;
        let t0 = std::time::Instant::now();
        let packet = encoder
            .encode_planes(&yb, yp, &uvb, uvp, i as u64, force)
            .with_context(|| format!("encode frame {i}"))?;
        let enc_ms = t0.elapsed().as_secs_f64() * 1000.0;
        total_bytes += packet.data.len();
        let t1 = std::time::Instant::now();
        let out = decoder
            .decode(i as u64, &packet.data)
            .with_context(|| format!("decode frame {i}"))?;
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
            let y_out = d.frame.read_nv12(width as usize, height as usize)?.y;
            min_psnr = min_psnr.min(psnr(&source_y, &y_out));
            if d.timestamp != i as u64 {
                println!(
                    "  WARN decoded timestamp {} for input {i}: decoder is not zero-latency",
                    d.timestamp
                );
            }
        }
    }
    let elapsed = start.elapsed().as_secs_f64();
    println!(
        "  {frames} frames, {total_bytes} bytes, {:.1} fps end to end, extradata {} bytes",
        frames as f64 / elapsed,
        encoder.parameter_sets().len()
    );
    status(
        decoded == frames,
        &format!("decoded {decoded}/{frames} frames with zero decoder latency"),
    );
    status(min_psnr > 30.0, &format!("min luma PSNR {min_psnr:.1} dB"));
    if decoded != frames || min_psnr <= 30.0 {
        bail!("round-trip check failed");
    }
    Ok(())
}

fn pipeline(
    target: &Target,
    node: &std::path::Path,
    output: Option<String>,
    gl: bool,
) -> Result<()> {
    use std::sync::mpsc;
    use std::time::Duration;
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
                captured = Some((frame.buffer.read_bgra()?, frame.buffer.clone()));
                break;
            }
            Ok(CaptureEvent::Error(e)) => bail!("capture error: {e}"),
            Ok(_) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(_) => break,
        }
    }
    drop(capturer);
    let (
        BgraImage {
            width: w,
            height: h,
            pixels: bgra,
        },
        buffer,
    ) = captured.ok_or_else(|| {
        anyhow!(
            "no frame captured (a headless output renders reliably; a physical KVM output may not)"
        )
    })?;
    println!("  captured {w}x{h} from {output}");

    if gl {
        return pipeline_gl(node, w, h, &bgra, &buffer);
    }

    let display = vaapi::open_display(node)?;
    let settings = EncoderSettings {
        width: w as u32,
        height: h as u32,
        bitrate: EncoderSettings::default_bitrate(w as u32, h as u32, 60),
        framerate: 60,
        low_power: false,
    };
    let mut encoder = DualEncoder::new(display.clone(), settings).context("dual encoder")?;
    let mut decoder = DualDecoder::new(display, w, h).context("dual decoder")?;

    let src444 = bgra_to_yuv444(&bgra, w * 4, w, h);
    let t0 = std::time::Instant::now();
    let packet = encoder.encode(&src444, 0, true).context("encode")?;
    let enc_ms = t0.elapsed().as_secs_f64() * 1000.0;
    println!(
        "  encoded main {} bytes, aux {} bytes, key={}, {enc_ms:.1} ms",
        packet.main.len(),
        packet.aux.len(),
        packet.keyframe
    );
    let t1 = std::time::Instant::now();
    let out444 = decoder
        .decode(0, &packet.main, &packet.aux)
        .context("decode")?
        .ok_or_else(|| anyhow!("dual decode produced no frame"))?;
    let dec_ms = t1.elapsed().as_secs_f64() * 1000.0;

    let y_psnr = psnr(&src444.y, &out444.y);
    let u_psnr = psnr(&src444.u, &out444.u);
    let v_psnr = psnr(&src444.v, &out444.v);
    let rgb_psnr = psnr(
        &rgb_channels(&bgra),
        &rgb_channels(&yuv444_to_bgra(&out444)),
    );
    println!("  decoded {dec_ms:.1} ms; PSNR Y {y_psnr:.1} U {u_psnr:.1} V {v_psnr:.1} dB, end-to-end RGB {rgb_psnr:.1} dB");
    status(
        y_psnr > 35.0 && u_psnr > 35.0 && v_psnr > 35.0,
        "Dual420 4:4:4 round-trip on a captured frame",
    );
    if !(y_psnr > 35.0 && u_psnr > 35.0 && v_psnr > 35.0) {
        bail!("4:4:4 pipeline PSNR too low");
    }
    Ok(())
}

fn pipeline_gl(
    node: &std::path::Path,
    w: usize,
    h: usize,
    bgra: &[u8],
    buffer: &hypr_capture::CaptureBuffer,
) -> Result<()> {
    use haver_codec::libva::{Display, Image, UsageHint, VA_FOURCC_NV12, VA_RT_FORMAT_YUV420};
    use haver_proto::chroma::{recombine_yuv444, split_yuv444, Nv12};
    use std::time::Instant;

    let info = &buffer.info;
    let src_fourcc = drm_fourcc::DrmFourcc::try_from(info.fourcc)
        .map_err(|_| anyhow!("captured fourcc {:#x} not a known DRM format", info.fourcc))?;
    let input = haver_gl::DmabufPlane {
        fd: info.fd.as_fd(),
        width: info.width,
        height: info.height,
        offset: info.planes[0].offset,
        stride: info.planes[0].stride,
        fourcc: src_fourcc,
        modifier: info.modifier,
    };
    println!(
        "  input dmabuf: {:?} {}x{} modifier {:#x}",
        src_fourcc, info.width, info.height, info.modifier
    );

    let display: std::rc::Rc<Display> = vaapi::open_display(node)?;
    let make_nv12 = || -> Result<_> {
        let mut s = display
            .create_surfaces::<()>(
                VA_RT_FORMAT_YUV420,
                Some(VA_FOURCC_NV12),
                w as u32,
                h as u32,
                Some(UsageHint::USAGE_HINT_ENCODER),
                vec![()],
            )
            .map_err(|e| anyhow!("create_surfaces: {e}"))?;
        Ok(s.remove(0))
    };
    let main_surf = make_nv12()?;
    let aux_surf = make_nv12()?;

    let mut headless = haver_gl::Headless::new(node)?;

    // Time the GL split over a few iterations. Re-export each iteration mirrors
    // the per-frame cost; the encoder would hold the surfaces across frames.
    let mut gl_ms = f64::INFINITY;
    for _ in 0..10 {
        let main_desc = main_surf
            .export_prime()
            .map_err(|e| anyhow!("export main: {e}"))?;
        let aux_desc = aux_surf
            .export_prime()
            .map_err(|e| anyhow!("export aux: {e}"))?;
        let (my, muv) = nv12_planes(&main_desc);
        let (ay, auv) = nv12_planes(&aux_desc);
        let t = Instant::now();
        headless
            .split_dual(&input, w as u32, h as u32, &my, &muv, &ay, &auv)
            .context("gl split")?;
        gl_ms = gl_ms.min(t.elapsed().as_secs_f64() * 1000.0);
    }

    // Read the two GL-filled surfaces back to a main/aux NV12 pair.
    let fmt = display
        .query_image_formats()
        .map_err(|e| anyhow!("{e}"))?
        .into_iter()
        .find(|f| f.fourcc == VA_FOURCC_NV12)
        .ok_or_else(|| anyhow!("no NV12 image format"))?;
    let read_nv12 = |surface: &haver_codec::libva::Surface<()>| -> Result<Nv12> {
        let image = Image::create_from(surface, fmt, (w as u32, h as u32), (w as u32, h as u32))
            .map_err(|e| anyhow!("map: {e}"))?;
        let d = image.as_ref();
        let va = *image.image();
        let (yo, uvo) = (va.offsets[0] as usize, va.offsets[1] as usize);
        let (yp, uvp) = (va.pitches[0] as usize, va.pitches[1] as usize);
        let mut nv = Nv12::new(w, h);
        for row in 0..h {
            nv.y[row * w..row * w + w].copy_from_slice(&d[yo + row * yp..yo + row * yp + w]);
        }
        for row in 0..h / 2 {
            nv.uv[row * w..row * w + w].copy_from_slice(&d[uvo + row * uvp..uvo + row * uvp + w]);
        }
        Ok(nv)
    };
    let gl_main = read_nv12(&main_surf)?;
    let gl_aux = read_nv12(&aux_surf)?;

    // CPU reference: the exact split the GPU path replaces. Take the best of
    // several warm runs so both sides are measured the same way.
    let cpu_src = bgra_to_yuv444(bgra, w * 4, w, h);
    let (cpu_main, cpu_aux) = split_yuv444(&cpu_src);
    let mut cpu_ms = f64::INFINITY;
    for _ in 0..10 {
        let t = Instant::now();
        let s = bgra_to_yuv444(bgra, w * 4, w, h);
        let _ = split_yuv444(&s);
        cpu_ms = cpu_ms.min(t.elapsed().as_secs_f64() * 1000.0);
    }

    // Correctness: recombine each and compare to the CPU 4:4:4 reference.
    let gl444 = recombine_yuv444(&gl_main, &gl_aux);
    let y_psnr = psnr(&cpu_src.y, &gl444.y);
    let u_psnr = psnr(&cpu_src.u, &gl444.u);
    let v_psnr = psnr(&cpu_src.v, &gl444.v);
    let my_psnr = psnr(&cpu_main.y, &gl_main.y);
    let muv_psnr = psnr(&cpu_main.uv, &gl_main.uv);
    let ay_psnr = psnr(&cpu_aux.y, &gl_aux.y);
    let auv_psnr = psnr(&cpu_aux.uv, &gl_aux.uv);
    println!("  per-plane PSNR vs CPU: main.Y {my_psnr:.1} main.UV {muv_psnr:.1} aux.Y {ay_psnr:.1} aux.UV {auv_psnr:.1} dB");
    println!("  recombined 4:4:4 PSNR vs CPU: Y {y_psnr:.1} U {u_psnr:.1} V {v_psnr:.1} dB");
    println!("  split time: GPU {gl_ms:.2} ms/frame  vs  CPU {cpu_ms:.2} ms/frame");

    // GL uses BT.709 float math and rounds slightly differently from the CPU
    // integer path, so exact equality is not expected; >40 dB means the shader
    // orientation and channel order are correct.
    let split_ok = y_psnr > 40.0 && u_psnr > 40.0 && v_psnr > 40.0;
    status(
        split_ok,
        "GPU 4:4:4 split matches CPU split (orientation + channel order)",
    );

    // Release the standalone split resources before the integrated encoder
    // builds its own GL context and surfaces.
    drop(headless);
    drop(main_surf);
    drop(aux_surf);

    // Integrated: encode the GPU-split surfaces, decode, recombine, and check
    // end-to-end fidelity against the captured frame.
    let settings = EncoderSettings {
        width: w as u32,
        height: h as u32,
        bitrate: EncoderSettings::default_bitrate(w as u32, h as u32, 60),
        framerate: 60,
        low_power: false,
    };
    let mut encoder =
        GlDualEncoder::new(display.clone(), node, settings).context("gl dual encoder")?;
    let mut decoder = DualDecoder::new(display, w, h).context("dual decoder")?;
    let packet = encoder.encode(&input, 0, true).context("gl encode")?;
    println!(
        "  gl-encoded main {} bytes, aux {} bytes, key={}",
        packet.main.len(),
        packet.aux.len(),
        packet.keyframe
    );
    let out444 = decoder
        .decode(0, &packet.main, &packet.aux)
        .context("decode")?
        .ok_or_else(|| anyhow!("dual decode produced no frame"))?;
    let e2e_psnr = psnr(&rgb_channels(bgra), &rgb_channels(&yuv444_to_bgra(&out444)));
    println!("  end-to-end RGB PSNR (GPU split -> encode -> decode): {e2e_psnr:.1} dB");
    let e2e_ok = e2e_psnr > 30.0;
    status(
        e2e_ok,
        "GPU-split Dual420 4:4:4 round-trip on a captured frame",
    );

    if !split_ok {
        bail!("GPU split PSNR too low: shader orientation or GR88 channel order is wrong");
    }
    if !e2e_ok {
        bail!("GPU-split end-to-end PSNR too low");
    }
    Ok(())
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
    use haver_proto::{ChromaMode, ClientCaps, ClientMsg, Codec, ServerMsg};
    use haver_transport::Framed;
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
        writer.write_msg(&ClientMsg::Hello { version: haver_proto::PROTOCOL_VERSION, keymap: String::new(), caps }).await?;
        let ack = reader.read_msg::<ServerMsg>().await?;
        let ServerMsg::HelloAck { session, outputs, .. } = ack else { bail!("expected HelloAck, got {ack:?}") };
        eprintln!("  HelloAck: headless={} output={} ({} outputs)", session.headless, session.output, outputs.len());
        let cfg = reader.read_msg::<ServerMsg>().await?;
        let (mut w, mut h, chroma) = match cfg { ServerMsg::StreamConfig { width, height, chroma, .. } => (width as usize, height as usize, chroma), o => bail!("expected StreamConfig, got {o:?}") };
        eprintln!("  StreamConfig: {w}x{h} chroma {chroma:?}");
        let display = vaapi::open_display(node)?;
        let mut decoder = Decoder::new(display.clone(), chroma, w, h)?;
        let mut got = 0usize;
        let mut keyframes = 0usize;
        while got < frames {
            let msg = reader.read_msg::<ServerMsg>().await?;
            match msg {
                ServerMsg::VideoFrame { frame_id, keyframe, data_len, aux_len, .. } => {
                    let main = reader.read_payload(data_len).await?;
                    let aux = if aux_len > 0 { reader.read_payload(aux_len).await?.to_vec() } else { Vec::new() };
                    if keyframe { keyframes += 1; }
                    let out = decoder.decode(frame_id, &main, &aux)?;
                    if out.is_some() { got += 1; }
                    if got == 1 { eprintln!("  first decoded frame ok ({}x{}, main {} aux {} bytes, key {keyframe})", w, h, data_len, aux_len); }
                    writer.write_msg(&ClientMsg::FrameAck { frame_id, decoded_at_ms: 0 }).await?;
                    if got == 3 { writer.write_msg(&ClientMsg::Resize { width: 800, height: 600, scale: 1.0 }).await?; }
                    if got == 4 {
                        if let Ok(text) = std::env::var("HAVER_SEND_CLIP") {
                            let bytes = text.into_bytes();
                            writer.write_msg_with_payloads(&ClientMsg::ClipboardData { mime_type: "text/plain;charset=utf-8".into(), offset: 0, total: bytes.len() as u64, data_len: bytes.len() as u32 }, &[&bytes]).await?;
                        }
                    }
                }
                ServerMsg::StreamConfig { width, height, chroma, .. } => {
                    w = width as usize; h = height as usize;
                    eprintln!("  reconfig to {w}x{h}");
                    decoder = Decoder::new(display.clone(), chroma, w, h)?;
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
    use haver_codec::color::{bgra_to_yuv444, yuv444_to_bgra};
    use haver_proto::chroma::{recombine_yuv444, split_yuv444, yuv444_to_nv12};
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

fn gltest(node: &std::path::Path) -> Result<()> {
    use haver_codec::libva::{Display, UsageHint, VA_FOURCC_NV12, VA_RT_FORMAT_YUV420};
    let (w, h) = (256u32, 256u32);
    let display: std::rc::Rc<Display> = vaapi::open_display(node)?;
    let mut surfaces = display
        .create_surfaces::<()>(
            VA_RT_FORMAT_YUV420,
            Some(VA_FOURCC_NV12),
            w,
            h,
            Some(UsageHint::USAGE_HINT_ENCODER),
            vec![()],
        )
        .map_err(|e| anyhow!("create_surfaces: {e}"))?;
    let surface = surfaces.remove(0);
    let desc = surface
        .export_prime()
        .map_err(|e| anyhow!("export_prime: {e}"))?;
    let layer = &desc.layers[0];
    let obj = &desc.objects[0];
    println!(
        "  VA NV12 surface exported: modifier {:#x}, Y off {} pitch {}",
        obj.drm_format_modifier, layer.offset[0], layer.pitch[0]
    );
    let y_plane = haver_gl::DmabufPlane {
        fd: obj.fd.as_fd(),
        width: w,
        height: h,
        offset: layer.offset[0],
        stride: layer.pitch[0],
        fourcc: drm_fourcc::DrmFourcc::R8,
        modifier: obj.drm_format_modifier,
    };
    let headless = haver_gl::Headless::new(node)?;
    let clear = headless.clear_plane(&y_plane, 0.5);
    match &clear {
        Ok(()) => println!("  GL cleared the surface Y plane to 0.5 (framebuffer complete)"),
        Err(e) => println!("  GL render into VA surface FAILED: {e}"),
    }
    // Read the surface back and check the Y plane is ~128.
    drop(desc); // close exported fds before mapping
    let fmt = display
        .query_image_formats()
        .map_err(|e| anyhow!("{e}"))?
        .into_iter()
        .find(|f| f.fourcc == VA_FOURCC_NV12)
        .ok_or_else(|| anyhow!("no NV12 image"))?;
    let image = haver_codec::libva::Image::create_from(&surface, fmt, (w, h), (w, h))
        .map_err(|e| anyhow!("map surface: {e}"))?;
    let data = image.as_ref();
    let yo = image.image().offsets[0] as usize;
    let samples: Vec<u8> = (0..8).map(|i| data[yo + i]).collect();
    let mean: f64 = (0..(w * h) as usize)
        .map(|i| data[yo + i] as f64)
        .sum::<f64>()
        / (w * h) as f64;
    println!("  read-back Y[0..8]={samples:?} mean={mean:.1} (expect ~128 if GL wrote it)");
    let ok = clear.is_ok() && (mean - 128.0).abs() < 8.0;
    status(ok, "GL render into VA encoder surface");
    if !ok {
        bail!("server GL-into-VA-surface path is not usable on this GPU");
    }
    Ok(())
}
