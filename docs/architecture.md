# haver architecture and design

haver remote-desktops a Hyprland session to another Hyprland machine over SSH.
This document covers what is built, the design choices behind it, and what is
still pending.

## Data flow

```
        server (remote host)                         client (local)
  ┌─────────────────────────────┐              ┌──────────────────────────────┐
  Hyprland output                │              │  GTK4 window (gtk::Picture)
    │ ext-image-copy-capture     │              │        ▲ MemoryTexture
    ▼                            │              │        │
  GBM dmabuf ─ CPU map ─► BGRA   │              │   BGRA ◄─ yuv444_to_bgra
    │                            │              │        ▲
    ▼ bgra_to_yuv444 (BT.709)    │              │   YUV 4:4:4 ◄ recombine
  YUV 4:4:4                      │   protocol   │        ▲
    │ split (AVC444)             │  over TCP    │   two NV12 ◄ two H.264 decoders
    ▼                            │   or ssh     │        ▲
  main NV12 + aux NV12           │  ═════════►  │        │ payloads
    │ two VA-API H.264 encoders  │              │   VideoFrame{main,aux}
    ▼ VideoFrame{main,aux} ──────┼──────────────┼────────┘
                                 │              │
  hypr-input ◄ Key/Pointer ◄─────┼──────────────┼──◄ GTK event controllers
```

Keyboard, pointer, resize, frame acks and keyframe requests flow client→server;
video, cursor and pongs flow server→client.

## Crates

| Crate | Role | `unsafe` |
|---|---|---|
| `haver-proto` | wire messages, framing header, AVC444 split/recombine, chroma up/subsample | none |
| `haver-transport` | `Framed` length-prefixed IO with out-of-band payloads (`write_vectored`), ssh spawn | none |
| `hypr-ipc` | Hyprland control socket (instance discovery, outputs, options) | none |
| `hypr-wl` | shared Wayland plumbing (connect, globals, output/seat tracking) | none |
| `hypr-capture` | output + cursor capture into GBM dmabufs on a calloop thread | none |
| `hypr-input` | virtual keyboard (xkb state) + virtual pointer on a calloop thread | none |
| `haver-codec` | VA-API H.264 encode/decode, CPU colour, Dual420/Single420, GPU-split encoder | none (GL via `haver-gl`) |
| `haver-gl` | GPU dmabuf import, AVC444 recombine shader (client), headless split into VA surfaces (server) | **yes, isolated here** |
| `haver-server` | ties capture+input+codec to the protocol; `--stdio`/`--listen` | none |
| `haver-client` | GTK4/libadwaita UI, decode worker | none |
| `haver-probe` | environment checks and the headless test client | none |
| `vendor/cros-*` | patched cros-libva and cros-codecs | in the libraries only |

All `unsafe` in haver is confined to the `haver-gl` crate (GL/EGL are C APIs). The GBM, VA-API and xkb crates wrap
the C libraries; the vendored codec libraries contain the only `unsafe`, and our
patches to them are documented in their `README.haver.md`.

## Key design choices

- **SSH is the only transport.** The server opens no port in production; the
  client spawns `ssh -T host haver-server --stdio` and the protocol runs over
  that pipe. `--listen`/`--connect` exist for local development only and have no
  auth. TCP framing is length-prefixed postcard messages; large video and cursor
  payloads ride outside the postcard body so the encoder output is sent with
  `write_vectored` and read straight into the decoder.

- **4:4:4 by two 4:2:0 streams (AVC444).** Hardware H.264 encoders only do
  4:2:0, which blurs coloured text. haver splits full 4:4:4 into a main stream
  (luma + even-position chroma) and an auxiliary stream (the dropped chroma), and
  recombines them bit-exactly on the client. Unlike RDP AVC444 v1 we do not
  average or filter the main chroma, because we own both ends, so the split and
  recombine are a lossless inverse pair (unit-tested through 1080p).
  `--low-bandwidth` drops to a single 4:2:0 stream with chroma upsampled on
  decode.

- **CBR, not constant-QP.** The AMD VA-API encoder mis-handles cros-codecs'
  constant-QP path (see `docs/hardware-quirks.md`); haver drives CBR.

- **Latest-wins, ack-paced.** The server keeps only the most recent captured
  frame and encodes it when the client has ack capacity. The number of unacked
  frames allowed is derived from a smoothed ack RTT and clamped to 2..8, so the
  frame rate is not capped by latency and a slow client cannot build a backlog.
  Frames are captured on demand, so a static screen costs nothing.

- **Threading.** cros-codecs' encoder and decoder use `Rc` and are `!Send`, so
  each runs on a single thread: the server loop and the client decode worker are
  current-thread tokio runtimes. Capture and input each own a Wayland connection
  on their own calloop thread and talk to the async side through channels; only
  plain buffers cross threads. On both peers, reads and writes run on separate
  tasks so an input burst cannot starve video and a blocked write cannot block
  reads.

- **Client rendering: GPU when available, CPU fallback.** At startup the client
  probes EGL dmabuf import (via a GBM render-node display). If supported, it
  shows a `gtk::GLArea` and the decoder hands the decoded NV12 dmabufs straight
  to a recombine shader (`haver-gl`) that reconstructs 4:4:4 and converts to RGB
  on the GPU, with no CPU pixel work. If not, it falls back to CPU recombine to
  BGRA in a `gdk::MemoryTexture`. All GL/EGL `unsafe` lives only in `haver-gl`.

- **Cursor.** The remote cursor is shown as the video widget's own cursor, so
  the local compositor draws it at the real pointer with no added latency; it is
  never baked into the video.

- **Clipboard (text).** The server bridges the compositor's text selection with
  `ext-data-control-v1` on its own thread; the client bridges `gdk::Clipboard`.
  A loop guard on each side stops a value it just set from bouncing back. Images
  and large non-text types are not carried.

## Testing

- **Unit tests** cover the pure logic: AVC444 split/recombine losslessness,
  single-stream subsample/upsample, BGRA↔YUV444 colour round-trip, NAL-header
  fixup safety, framing with payloads and partial writes, the ack-window bounds,
  keymap building, and Hyprland instance discovery.
- **`haver-probe`** is the hardware integration harness: `protocols`, `outputs`,
  `vaapi`, `roundtrip` (encode→decode PSNR), `capture`, `input`, `pipeline`
  (capture→4:4:4→decode), and `serve-test` (a headless protocol client).
- **`scripts/e2e.sh`** boots a nested Hyprland and asserts PASS across the probe
  checks, the 4:4:4 pipeline, and both Dual420 and Single420 server-plus-client
  streams. It needs a Hyprland session and a VA-API GPU, so it is not a CI unit
  test; run it on a target machine.

## Pending and recommended improvements

Built and validated on AMD: capture, input, H.264 encode/decode, Dual420 and
Single420, the server loop, the GTK client, keymap upload, remote cursor,
shortcut inhibit, reconnect, and the text clipboard bridge (both directions,
tested against wl-clipboard).

Not yet built, roughly in priority order:

1. **Wire the server GPU split into the live session.** `GlDualEncoder` is built,
   verified end-to-end, and measured (~2x faster than the CPU split, growing with
   resolution). It is not yet the server's live encoder because that needs the
   captured dmabuf held on the encoder thread until `glFinish` returns. See the
   decision under "GPU acceleration" above. VPP stays a separate Single420-only
   option.
2. **Native single-stream 4:4:4, and AV1/HEVC.** Probe-gated; this GPU exposes
   no such VA-API encode entrypoint, so they cannot be validated here. Needs an
   Intel or newer GPU.
3. **Verified ssh path** from a cold machine, including the `WAYLAND_DISPLAY` /
   `XDG_RUNTIME_DIR` environment setup, and a systemd user unit if wanted.
4. **Polish**: multi-output selection UI, a configurable escape key for shortcut
   inhibit, image clipboard, and `tc netem` tuning of the adaptive ack window.

See `docs/hardware-quirks.md` for driver-specific behaviour and the low-severity
items surfaced by code review.

## GPU acceleration: measurements and the path decision

The CPU colour/split passes are the work a GPU path removes. Measured on the
AMD Ryzen 9955HX iGPU (Mesa radeonsi), rayon-parallel, via `haver-probe bench`:

| Stage | 1280x720 | 1920x1080 | 3840x2160 |
|---|---|---|---|
| BGRA→YUV444 (server) | 0.7 ms | 1.0 ms | 6.2 ms |
| split 4:4:4→2×NV12 (server, Dual420) | 0.3 ms | 0.6 ms | 3.0 ms |
| subsample 4:4:4→NV12 (server, Single420) | 0.2 ms | 0.5 ms | 3.0 ms |
| recombine 2×NV12→4:4:4 (client) | 0.3 ms | 0.6 ms | 3.0 ms |
| YUV444→BGRA (client) | 0.7 ms | 1.5 ms | 5.5 ms |

So at 1080p60 the CPU cost is ~1.6 ms/frame server and ~2.1 ms client — a
modest slice of the 16.6 ms budget. At 4K it is ~9 ms server and ~8.5 ms client,
a real bottleneck (and these are parallel wall-clock times, so more core-time).

**Client display path — two approaches, both built.** GPU (GL recombine shader
in `haver-gl`) when EGL dmabuf import is available, else CPU. The GPU path is
auto-selected, self-corrects to CPU at run time if GL rendering fails, and
removes all client pixel work. This is the broadest, highest-value win and is
verified running on radeonsi (visual confirmation pending a real display).

**Server path — CPU only, by measured decision.** We evaluated three options:

1. **CPU (built).** Universal; cost as measured above.
2. **VA-API VPP.** `haver-probe`/ffmpeg confirmed radeonsi VPP does BGRA→NV12,
   into a VA-native surface the encoder reads with no copy — a full GPU path
   *for Single420 only*. VPP is a colour-convert/scaler and **cannot do the
   AVC444 split**, so it does nothing for the default Dual420 path. It also is
   not reachable through the vendored cros-libva 0.0.12: its bindings omit the
   VPP structs (they need `va_vpp.h` added and bindings regenerated, plus the
   `proc_pipeline` wrapper ported and a VPP context wired).
3. **GL split on the server (built and measured).** This does the whole
   BGRA→YUV444→2×NV12 split on the GPU and helps the default Dual420 path.
   radeonsi refuses an *external* dmabuf as encoder input, but it does not need
   one: GL renders into the encoder's **own** VA input surfaces. Each surface is
   exported with `vaExportSurfaceHandle` (composed NV12 layer), imported back as
   two EGL images (Y as `R8`, UV as `GR88`, with the tiled surface's DRM
   modifier), and four fragment shaders write the main/aux Y and UV planes. The
   encoder then reads the same surfaces with no upload. It lives in
   `haver-codec::gl_split::GlDualEncoder`, all `unsafe` still confined to
   `haver-gl` (`Headless` context + `split_dual`).

   Verified end-to-end on radeonsi via `haver-probe pipeline --gl`: the GPU split
   matches the CPU split at 65-74 dB per plane (float-vs-integer rounding only),
   and the full GPU-split→encode→decode→recombine round-trip reaches the same
   RGB PSNR as the CPU pipeline (~45 dB on a captured desktop frame). Split time,
   best of ten warm runs, GPU vs CPU:

   | Resolution | GPU split | CPU split |
   |---|---|---|
   | 1280x720 | 0.59 ms | 1.19 ms |
   | 1920x1080 | 1.16 ms | 2.75 ms |
   | 3840x2160 | 4.82 ms | 9.01 ms |

   The GPU figure includes a per-frame input import and a blocking `glFinish`;
   in a pipelined server the destination surfaces are imported once and the GPU
   work overlaps other CPU work, so the effective server CPU saved is close to
   the full CPU-split column. The win is about 2x in wall-clock and grows with
   resolution.

Decision: keep **CPU on the server as the default**, with the GPU split built,
measured, and ready as an opt-in — the client keeps its GPU/CPU pair. The server
split is not yet wired into the live session because it needs the captured
dmabuf to stay readable on the encoder thread until `glFinish` returns, which
means holding the capture-ring frame across the capture→encoder thread boundary
(or dup-ing its fd and pinning the ring slot) rather than the current
map-to-BGRA-and-release. That is a capture-lifecycle change with a small
regression risk to the one working path, and the 1080p gain is modest, so the
integration is left as a deliberate switch to flip. VPP remains a separate,
Single420-only option (add `va_vpp.h` to the vendored cros-libva wrapper, port
`proc_pipeline`); the GL split is the better Dual420 answer and is now the
recommended server GPU path when server CPU (especially at 4K) matters.

## Dependencies and binaries

The binaries are dynamically linked (not static): `haver-server`/`haver-probe`
need ~11 system libraries (libva, libgbm, libdrm, libwayland-client,
libxkbcommon, libc) and libva `dlopen`s the GPU's VA driver; `haver-client`
additionally pulls the full GTK4 runtime (~137 libraries). A normal
Omarchy/Hyprland desktop already has all of these (they are the PKGBUILD
`depends`). Full static linking is impractical: libva loads its driver by
`dlopen`, and Mesa EGL/GBM and GTK/GObject are not built for static linking.
The crate graph is ~167 crates, normal for a GTK4 + async + bindgen-FFI stack;
it stays lean by avoiding GStreamer/ffmpeg and using its own protocol.
