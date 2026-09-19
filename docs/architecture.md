# gliff architecture and design

gliff remote-desktops a Hyprland session to another Hyprland machine over SSH.
This document covers what is built, the design choices behind it, and what is
still pending.

## Data flow

```
        server (remote host)                         client (local)
  ┌─────────────────────────────┐              ┌──────────────────────────────┐
  Hyprland output                │              │  GTK4 window (gtk::Picture)
    │ ext-image-copy-capture     │              │        ▲ GdkDmabufTexture
    ▼                            │              │        │
  GBM dmabuf ─ Vulkan import ─►  │              │   BGRX dmabuf (Vulkan export)
    │ VkImage (sampled)          │              │        ▲
    ▼ split.comp (BT.709 + AVC444)│             │   recombine.comp
  main NV12 + aux NV12           │   protocol   │        ▲
    │ (encoder input images)     │  over TCP    │   two NV12 ◄ two Vulkan decoders
    ▼ two VK_KHR_video_encode_h264│   or ssh     │        ▲
  VideoFrame{main,aux} ──────────┼──────────────┼────────┘ payloads
                                 │              │
  hypr-input ◄ Key/Pointer ◄─────┼──────────────┼──◄ GTK event controllers
```

Nothing touches pixels on the CPU. On the server the captured dmabuf is
imported once per ring buffer, the split shader writes the encoders' input
images, and the encoders read them in place; only the coded bytes come back to
the CPU. On the client the two decoders write NV12 images, the recombine shader
writes a linear BGRX image, and GTK imports that image as a dmabuf texture.

Keyboard, pointer, resize, frame acks and keyframe requests flow client→server;
video, cursor and pongs flow server→client.

## Crates

| Crate | Role | `unsafe` |
|---|---|---|
| `gliff-proto` | wire messages, framing header, CPU reference for colour and the AVC444 split/recombine | none |
| `gliff-transport` | `Framed` length-prefixed IO with out-of-band payloads (`write_vectored`), ssh spawn | none |
| `hypr-ipc` | Hyprland control socket (instance discovery, outputs, options) | none |
| `hypr-wl` | shared Wayland plumbing (connect, globals, output/seat tracking, calloop runner) | none |
| `hypr-capture` | output + cursor capture into GBM dmabufs on a calloop thread | none |
| `hypr-input` | virtual keyboard (xkb state) + virtual pointer + text clipboard on calloop threads | none |
| `gliff-vk` | Vulkan device, dmabuf import/export, split/recombine compute, H.264 encode/decode, header parser | **yes, isolated here** |
| `gliff-server` | ties capture+input+encoder to the protocol; `--stdio`/`--listen` | none |
| `gliff` | GTK4/libadwaita UI, decode worker | one block: hands GTK a dmabuf fd |
| `gliff-probe` | environment checks and the headless test client | none |

`gliff-vk` wraps `ash`, whose every call is `unsafe` because Vulkan is a C API
with no lifetime or synchronisation checks. The crate exposes plain Rust types
(`Gpu`, `Encoder`, `Decoder`, `DisplayFrame`); every `unsafe` block carries a
`SAFETY` comment, and the Khronos validation layer runs clean on the probe
round-trip.

## Key design choices

- **SSH is the only transport.** The server opens no port in production; the
  client spawns `ssh -T host gliff-server --stdio` and the protocol runs over
  that pipe. `--listen`/`--connect` exist for local development only and have no
  auth. TCP framing is length-prefixed postcard messages; large video and cursor
  payloads ride outside the postcard body so the encoder output is sent with
  `write_vectored` and read straight into the decoder.

- **4:4:4 by two 4:2:0 streams (AVC444).** Hardware H.264 encoders only do
  4:2:0, which blurs coloured text. gliff splits full 4:4:4 into a main stream
  (luma + even-position chroma) and an auxiliary stream (the dropped chroma), and
  recombines them bit-exactly on the client. Unlike RDP AVC444 v1 we do not
  average or filter the main chroma, because we own both ends, so the split and
  recombine are a lossless inverse pair. The CPU implementation in
  `gliff-proto::chroma` is unit-tested through 1080p and is the oracle the
  compute shaders are checked against (`gliff-probe pipeline`).
  `--low-bandwidth` drops to a single 4:2:0 stream with chroma upsampled on
  decode.

- **One GPU API.** Import, colour conversion, split, encode, decode, recombine
  and export all happen in Vulkan on one device, ordered by a timeline
  semaphore. Buffers never cross between GPU APIs, and driver differences are
  read from the Vulkan capability queries rather than special-cased; the same
  code runs on any driver with Vulkan Video.

- **Low-delay H.264.** The encoder emits IDR then P frames with one reference
  and no reordering (POC type 0), High profile, CABAC, CBR at the configured
  bitrate, with the SPS and PPS prepended to every IDR so any keyframe is a
  random-access point. The decoder parses only what the hardware does not
  (SPS, PPS, slice header up to the reference marking) and manages a two-slot
  DPB with sliding-window marking.

- **Latest-wins, ack-paced, budget-adapted.** The server keeps only the most
  recent captured frame and encodes it when the link can take it: the client
  has ack capacity, the previous frame has left for the wire, and the pace
  slot (`--max-fps`, default 60) has come. The next capture is requested
  before the current frame encodes, so the compositor works while the GPU
  does, and capture continues while the link is blocked so the frame that
  goes out is the newest. The number of unacked frames allowed follows the
  base (minimum) ack RTT, clamped to 2..8, so queueing delay cannot widen it.
  The rate control is a per-frame bit budget (0.1 bits per pixel per stream
  at most) programmed as CBR at `budget x pace`, where the pace is the frame
  rate the encoder is measured to sustain, so the nominal bitrate is what is
  really sent. Queueing (the window-minimum RTT over the base) cuts the
  budget to 85% of the rate acks arrive at, in one step; a quiet link grows
  it back, fast at first. Frames are captured on demand, so a static screen
  costs nothing.

- **Threading.** Each pipeline lives on one thread: the server loop and the
  client decode worker are current-thread tokio runtimes that own their
  `gliff-vk` objects. Capture and input each own a Wayland connection on their
  own calloop thread and talk to the async side through channels; the capture
  thread hands the whole `CapturedFrame` (its dmabuf) to the encoder, which
  releases the ring slot when the encode has finished. On the server the socket
  reader is its own thread and injects key and pointer events directly, so
  input never waits behind an encode (~30 ms at 4K) or a blocked write;
  writes run on a task. On the client the decode worker lands each frame in
  a slot and wakes the GTK loop, so no frame waits for a timer.

- **Client display.** The decode worker exports each finished BGRX image as a
  linear dmabuf and the UI wraps it in a `GdkDmabufTexture` on a
  `gtk::Picture`. GTK imports the dmabuf itself (through its GL/Vulkan renderer
  or, failing that, a CPU map), so the client needs no GL code and no fallback
  path of its own.

- **Cursor.** The remote cursor is shown as the video widget's own cursor, so
  the local compositor draws it at the real pointer with no added latency; it is
  never baked into the video.

- **Clipboard (text).** The server bridges the compositor's text selection with
  `ext-data-control-v1` on its own thread; the client bridges `gdk::Clipboard`.
  A loop guard on each side stops a value it just set from bouncing back. Images
  and large non-text types are not carried.

## Testing

- **Unit tests** cover the pure logic: AVC444 split/recombine losslessness,
  single-stream subsample/upsample, BGRA↔YUV444 colour round-trip, the H.264
  header parser against an x264 stream, framing with payloads and partial
  writes, the ack-window bounds, keymap building, and Hyprland instance
  discovery.
- **`gliff-probe`** is the hardware integration harness: `protocols`,
  `outputs`, `vulkan`, `roundtrip` (synthetic BGRA → encode → decode → PSNR
  against the CPU reference), `capture`, `input`, `pipeline` (a captured
  dmabuf through the exact server and client pipelines), and `serve-test` (a
  headless protocol client). Run it under `VK_LAYER_KHRONOS_validation` after
  touching `gliff-vk`.
- **`scripts/e2e.sh`** boots a nested Hyprland and asserts PASS across the probe
  checks, the GPU pipeline, both Dual420 and Single420 server-plus-client
  streams, and the clipboard. It needs a Hyprland session and a GPU with Vulkan
  Video, so it is not a CI unit test; run it on a target machine.

## Measurements

On the AMD Ryzen 9955HX iGPU (RADV, Mesa 26.2), release build, 1920x1080
Dual420, from `gliff-probe roundtrip`:

| Stage | Time per frame |
|---|---|
| split + two encodes (both submitted, then waited; one encode queue) | ~8.5 ms |
| two decodes + recombine (GPU, one fence wait) | ~5 ms |
| CPU readback of the BGRX frame (probe only, cached host memory) | ~1.5 ms |

At 3840x2160 Dual420 each encode takes ~14 ms and the two run one after the
other on the single encode queue, so a frame costs ~29 ms of encode; the
decode side takes 12-25 ms. The encode time does not depend on the bitrate.

End to end, with `scripts/bench.sh` (server and `gliff-probe stream-bench`
on one machine, so both ends share the GPU; a nested Hyprland with a 4K
headless output showing 30 fps moving content):

| Case | Before | After |
|---|---|---|
| 4K, open link: frame rate | 18 fps, 61 ms between frames | 30 fps (the content rate), 33 ms ± 1 |
| 4K, open link: capture-to-decoded latency | 26 ms | 12.5 ms |
| 4K, 20 Mbit/s link with 10 ms delay: latency p50 / p95 / max | 39 / 313 / 424 ms | 46 / 115 / 156 ms |
| 4K, 20 Mbit/s link: link use | 9 Mbit/s, oscillating | 19 Mbit/s |

No pixel work happens on the CPU at any resolution; the remaining cost is the
encode hardware itself.

## Pending and recommended improvements

Built and validated on AMD: capture, input, Vulkan H.264 encode/decode,
Dual420 and Single420, the server loop, the GTK client, keymap upload, remote
cursor, shortcut inhibit, reconnect, and the text clipboard bridge.

Not yet built, roughly in priority order:

1. **Test on Intel ANV and NVIDIA.** Everything runs on one AMD RADV
   machine; a second driver decides whether the per-driver freedom is real.
2. **Native single-stream 4:4:4, and AV1/HEVC.** Vulkan Video exposes HEVC
   and AV1 profiles; a 4:4:4 profile on some driver would retire the split.
3. **Verified ssh path** from a cold machine, including the `WAYLAND_DISPLAY` /
   `XDG_RUNTIME_DIR` environment setup, and a systemd user unit if wanted.
4. **Polish**: multi-output selection UI, image clipboard, and `tc netem` tuning
   of the adaptive ack window.

See `docs/hardware-quirks.md` for driver-specific behaviour and the low-severity
items surfaced by code review.

## Dependencies and binaries

The binaries are dynamically linked: `gliff-server`/`gliff-probe` need
libvulkan, libgbm, libdrm, libwayland-client, libxkbcommon and libc, and the
Vulkan loader `dlopen`s the GPU's ICD; `gliff` additionally pulls the
full GTK4 runtime. A normal Hyprland desktop already has all of these (they are
the PKGBUILD `depends`). The Rust side is `ash` (thin generated bindings, no C
build step) plus the Wayland, GTK and async crates. The compute shaders are
committed as SPIR-V, so no shader compiler is needed to build; rerun
`crates/gliff-vk/shaders/build.sh` (needs `glslc`) after editing a shader.
